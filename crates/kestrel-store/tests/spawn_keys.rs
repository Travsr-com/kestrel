//! Issue #92, store side: every spawn of a parent is kept with a key made from its agent and its
//! input, so a parent started again (after a restart) that spawns the same thing gets the child it
//! spawned before instead of a new one. The key survives a reopen and the v5 to v6 migration fills
//! it in for children spawned before.
//!
//! `spawn_child_once(parent, plan, nth, ..)` returns the `nth` (from 0) child of `parent` with the
//! plan's key when there is one, oldest first, and spawns a new child otherwise. A child it returns
//! again is not spawned again: no carve, no event, and it does not count against `max_children`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use kestrel_store::{
    ChildPlan, ControlCommand, SCHEMA_VERSION, SpawnOnce, SpawnRefusal, Store, StoreError,
    spawn_key,
};
use kestrel_types::{AgentSpec, EventKind, MicroUsd, RunId, RunStatus};
use tempfile::TempDir;

const DOLLAR: i64 = 1_000_000;

fn agent(name: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: opencode\nrole: r\nmodel_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 5.00, wall_clock: 10m }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture parses")
}

fn run(store: &Store, id: RunId) {
    store
        .transition(id, &RunStatus::Queued, RunStatus::Provisioning)
        .unwrap();
    store
        .transition(id, &RunStatus::Provisioning, RunStatus::Running)
        .unwrap();
}

fn root(store: &Store) -> RunId {
    let id = store.create_run(&agent("root"), "go").unwrap();
    run(store, id);
    id
}

fn plan<'a>(spec: &'a AgentSpec, input: &'a str, max_children: u32) -> ChildPlan<'a> {
    ChildPlan {
        spec,
        input,
        budget: Some(MicroUsd(DOLLAR / 10)),
        max_children,
        max_tree_runs: 100,
        child_events: Vec::new(),
    }
}

fn once(
    store: &Store,
    parent: RunId,
    spec: &AgentSpec,
    input: &str,
    nth: usize,
) -> Result<SpawnOnce, StoreError> {
    store.spawn_child_once(parent, plan(spec, input, 10), nth, |_, _| Ok(None))
}

fn new_child(outcome: Result<SpawnOnce, StoreError>) -> RunId {
    match outcome {
        Ok(SpawnOnce::New(spawned)) => spawned.id,
        other => panic!("expected a new child, got {other:?}"),
    }
}

fn existing(outcome: Result<SpawnOnce, StoreError>) -> RunId {
    match outcome {
        Ok(SpawnOnce::Existing(id)) => id,
        other => panic!("expected an existing child, got {other:?}"),
    }
}

fn spawned_events(store: &Store, parent: RunId) -> usize {
    store
        .events_since(parent, 0)
        .unwrap()
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ChildSpawned { .. }))
        .count()
}

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

// A spawn's key is kept with the child and survives closing and opening the store: the same agent
// and input from the same parent get the child spawned before, with nothing carved or recorded.
#[test]
fn a_spawn_key_survives_a_reopen_and_returns_the_same_child() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("kestrel.db");
    let worker = agent("worker");
    let (parent, child) = {
        let store = Store::open(&path).unwrap();
        let parent = root(&store);
        let child = store
            .spawn_child(parent, plan(&worker, "help", 10), |_, _| Ok(None))
            .unwrap()
            .id;
        (parent, child)
    };
    let store = Store::open(&path).unwrap();
    let limit = store.limit(parent).unwrap();
    assert_eq!(existing(once(&store, parent, &worker, "help", 0)), child);
    assert_eq!(store.children(parent).unwrap().len(), 1);
    assert_eq!(spawned_events(&store, parent), 1);
    assert_eq!(store.limit(parent).unwrap(), limit, "nothing carved again");
}

// The nth spawn with a key maps to the nth child with it, oldest first; past the children there
// are, a spawn is new.
#[test]
fn the_nth_spawn_with_a_key_is_the_nth_child_with_it() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store);
    let worker = agent("worker");
    let first = new_child(once(&store, parent, &worker, "help", 0));
    let second = new_child(once(&store, parent, &worker, "help", 1));
    assert_ne!(first, second);
    assert_eq!(existing(once(&store, parent, &worker, "help", 0)), first);
    assert_eq!(existing(once(&store, parent, &worker, "help", 1)), second);
    let third = new_child(once(&store, parent, &worker, "help", 2));
    assert_eq!(store.children(parent).unwrap().len(), 3);
    assert_eq!(existing(once(&store, parent, &worker, "help", 2)), third);
    // Asking past the next one still spawns just one more.
    let fourth = new_child(once(&store, parent, &worker, "help", 9));
    assert_eq!(existing(once(&store, parent, &worker, "help", 3)), fourth);
}

// Another input, another agent or another parent is another spawn.
#[test]
fn a_key_is_its_agent_its_input_and_its_parent() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store);
    let other_parent = root(&store);
    let worker = agent("worker");
    let helper = agent("helper");
    let child = new_child(once(&store, parent, &worker, "help", 0));
    assert_ne!(new_child(once(&store, parent, &worker, "other", 0)), child);
    assert_ne!(new_child(once(&store, parent, &helper, "help", 0)), child);
    assert_ne!(
        new_child(once(&store, other_parent, &worker, "help", 0)),
        child
    );
    assert_eq!(existing(once(&store, parent, &worker, "help", 0)), child);
}

// A child returned again does not count against `max_children`, finished or not; a new spawn past
// the limit is still refused.
#[test]
fn a_child_returned_again_does_not_count_twice() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store);
    let worker = agent("worker");
    let child = store
        .spawn_child(parent, plan(&worker, "help", 1), |_, _| Ok(None))
        .unwrap()
        .id;
    run(&store, child);
    store
        .transition(child, &RunStatus::Running, RunStatus::Completed)
        .unwrap();
    let again = store.spawn_child_once(parent, plan(&worker, "help", 1), 0, |_, _| Ok(None));
    assert_eq!(existing(again), child);
    let past = store.spawn_child_once(parent, plan(&worker, "help", 1), 1, |_, _| Ok(None));
    assert!(
        matches!(
            past,
            Err(StoreError::Spawn(SpawnRefusal::TooManyChildren(1)))
        ),
        "{past:?}"
    );
    assert_eq!(store.children(parent).unwrap().len(), 1);
}

// Only a running parent spawns, also when the spawn would return a child it has.
#[test]
fn a_parent_that_is_not_running_gets_no_child_back() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store);
    let worker = agent("worker");
    new_child(once(&store, parent, &worker, "help", 0));
    store
        .transition(parent, &RunStatus::Running, RunStatus::Cancelled)
        .unwrap();
    let refused = once(&store, parent, &worker, "help", 0);
    assert!(
        matches!(
            refused,
            Err(StoreError::Spawn(SpawnRefusal::ParentNotRunning(..)))
        ),
        "{refused:?}"
    );
    assert!(matches!(
        once(&store, RunId::new(), &worker, "help", 0),
        Err(StoreError::NotFound(_))
    ));
}

// The key is a digest of the agent and the input, so long inputs are not stored twice and an agent
// name cannot run into its input.
#[test]
fn a_spawn_key_is_a_stable_digest() {
    let key = spawn_key("worker", "help");
    assert_eq!(key, spawn_key("worker", "help"));
    assert_eq!(key.len(), 64);
    assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(spawn_key("ab", "c"), spawn_key("a", "bc"));
    assert_ne!(key, spawn_key("worker", "help "));
}

// A database from before the keys (v5) gets them for every child it has when it is opened, so a
// parent started again after the upgrade gets its children back too.
#[test]
fn migrating_a_v5_database_keys_the_children_it_has() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("kestrel.db");
    let worker = agent("worker");
    let (parent, first, second) = {
        let store = Store::open(&path).unwrap();
        let parent = root(&store);
        let spawn = |input| {
            store
                .spawn_child(parent, plan(&worker, input, 10), |_, _| Ok(None))
                .unwrap()
                .id
        };
        (parent, spawn("help"), spawn("help"))
    };
    // What a v5 database looks like: the same tables, without the keys.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE spawn_keys; PRAGMA user_version = 5;")
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(SCHEMA_VERSION, 6);
    assert_eq!(user_version(&path), 6);
    assert_eq!(existing(once(&store, parent, &worker, "help", 0)), first);
    assert_eq!(existing(once(&store, parent, &worker, "help", 1)), second);
    new_child(once(&store, parent, &worker, "help", 2));
}

// A command pushed "once" is not pushed again while the same one waits unclaimed for the run.
#[test]
fn a_command_pushed_once_waits_only_once() {
    let store = Store::open_in_memory().unwrap();
    let a = root(&store);
    let b = root(&store);
    assert!(store.push_control_once(a, ControlCommand::Resume).unwrap());
    assert!(!store.push_control_once(a, ControlCommand::Resume).unwrap());
    assert!(store.push_control_once(b, ControlCommand::Resume).unwrap());
    assert!(store.push_control_once(a, ControlCommand::Pause).unwrap());
    let taken = store.take_control().unwrap();
    assert_eq!(taken.len(), 3);
    // Claimed: a new one may be pushed.
    assert!(store.push_control_once(a, ControlCommand::Resume).unwrap());
    assert!(matches!(
        store.push_control_once(RunId::new(), ControlCommand::Resume),
        Err(StoreError::NotFound(_))
    ));
}
