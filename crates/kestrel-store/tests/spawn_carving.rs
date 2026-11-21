//! Issue #36, store side: `spawn_child` carves a child's budget out of its parent's remaining
//! budget in one transaction, and a finished child's unspent budget goes back up the tree.
//!
//! Model: a run may spend up to its own `limit`. A spawn moves `carve` from the parent's limit to
//! the child's; a finished child keeps what it spent and returns the rest. So the limits of a
//! tree always add up to the root budget. Each run can still go past its own limit by one
//! in-flight call, so the tree can go past the root budget by those overshoots, never more.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Barrier};
use std::thread;

use kestrel_store::{ChildPlan, SpawnRefusal, Spawned, Store, StoreError};
use kestrel_types::{AgentSpec, EventKind, MicroUsd, ModelCallRecord, RunId, RunStatus};
use proptest::prelude::*;
use tempfile::TempDir;

const DOLLAR: i64 = 1_000_000;

fn agent(name: &str, usd: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: opencode\nrole: r\nmodel_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: {usd}, wall_clock: 10m }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture parses")
}

fn usd(micro: i64) -> MicroUsd {
    MicroUsd(micro)
}

/// Moves a `Queued` run to `Running`, the only status a parent can spawn from.
fn run(store: &Store, id: RunId) {
    store
        .transition(id, &RunStatus::Queued, RunStatus::Provisioning)
        .unwrap();
    store
        .transition(id, &RunStatus::Provisioning, RunStatus::Running)
        .unwrap();
}

fn finish(store: &Store, id: RunId) {
    store
        .transition(id, &RunStatus::Running, RunStatus::Completed)
        .unwrap();
}

/// A running root with `usd` budget.
fn root(store: &Store, usd: &str) -> RunId {
    let id = store.create_run(&agent("root", usd), "go").unwrap();
    run(store, id);
    id
}

fn spend(store: &Store, run: RunId, micro: i64) {
    store
        .record_model_call(&ModelCallRecord {
            run_id: run,
            provider: "anthropic".into(),
            model: "m".into(),
            tokens_in: 1,
            tokens_out: 1,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            cost: Some(MicroUsd(micro)),
            latency_ms: 1,
            status: 200,
        })
        .unwrap();
}

fn plan<'a>(spec: &'a AgentSpec, budget: Option<MicroUsd>) -> ChildPlan<'a> {
    ChildPlan {
        spec,
        input: "child input",
        budget,
        max_children: 10,
        max_tree_runs: 100,
        child_events: Vec::new(),
    }
}

/// Spawns with no capability token.
fn spawn(store: &Store, parent: RunId, budget: Option<MicroUsd>) -> Result<Spawned, StoreError> {
    let spec = agent("child", "5.00");
    store.spawn_child(parent, plan(&spec, budget), |_, _| Ok(None))
}

fn refusal(result: Result<Spawned, StoreError>) -> SpawnRefusal {
    match result {
        Err(StoreError::Spawn(r)) => r,
        other => panic!("expected a spawn refusal, got {other:?}"),
    }
}

fn run_count(store: &Store) -> usize {
    store.list_runs(Default::default()).unwrap().len()
}

// 1. Given a parent with $2 remaining and a spawn of $0.50, then the child's budget is $0.50 and
//    the parent's remaining is $1.50.
#[test]
fn s1_spawn_carves_the_child_budget_out_of_the_parent() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let child = spawn(&store, parent, Some(usd(DOLLAR / 2))).unwrap();
    assert_eq!(child.budget, usd(DOLLAR / 2));
    assert_eq!(store.limit(child.id).unwrap(), usd(DOLLAR / 2));
    assert_eq!(store.remaining(child.id).unwrap(), usd(DOLLAR / 2));
    assert_eq!(store.remaining(parent).unwrap(), usd(3 * DOLLAR / 2));
    let row = store.get_run(child.id).unwrap().unwrap();
    assert_eq!(row.parent, Some(parent));
    assert_eq!(row.status, RunStatus::Queued);
}

// 1, with spend: remaining is the limit minus what the parent itself spent.
#[test]
fn s1_remaining_counts_the_parents_own_spend() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    spend(&store, parent, DOLLAR / 4);
    spawn(&store, parent, Some(usd(DOLLAR / 2))).unwrap();
    assert_eq!(store.remaining(parent).unwrap(), usd(DOLLAR + DOLLAR / 4));
}

// 2. Given a parent with $0.40 remaining and a spawn asking $0.50, then it fails with a budget
//    refusal and nothing is created or reserved.
#[test]
fn s2_a_spawn_over_the_remaining_budget_creates_and_reserves_nothing() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "0.40");
    let before = store.events_since(parent, 0).unwrap().len();
    let r = refusal(spawn(&store, parent, Some(usd(DOLLAR / 2))));
    assert_eq!(
        r,
        SpawnRefusal::Budget {
            asked: usd(DOLLAR / 2),
            remaining: usd(4 * DOLLAR / 10)
        }
    );
    assert_eq!(run_count(&store), 1);
    assert_eq!(store.remaining(parent).unwrap(), usd(4 * DOLLAR / 10));
    assert_eq!(store.events_since(parent, 0).unwrap().len(), before);
}

#[test]
fn s2_a_zero_or_negative_ask_is_refused() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "1.00");
    for ask in [0, -1] {
        assert!(matches!(
            refusal(spawn(&store, parent, Some(usd(ask)))),
            SpawnRefusal::Budget { .. }
        ));
    }
    assert_eq!(run_count(&store), 1);
}

// Default carve: the smaller of the child spec's budget and half the parent's remaining.
#[test]
fn default_carve_is_the_smaller_of_the_spec_budget_and_half_the_remaining() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let small = agent("small", "0.30");
    let a = store
        .spawn_child(parent, plan(&small, None), |_, _| Ok(None))
        .unwrap();
    assert_eq!(a.budget, usd(3 * DOLLAR / 10));
    // 1.70 left: half is 0.85, less than the big spec's 5.00.
    let b = spawn(&store, parent, None).unwrap();
    assert_eq!(b.budget, usd(85 * DOLLAR / 100));
}

#[test]
fn default_carve_of_nothing_is_refused() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "1.00");
    spend(&store, parent, DOLLAR);
    assert!(matches!(
        refusal(spawn(&store, parent, None)),
        SpawnRefusal::Budget { .. }
    ));
}

// 3. Given two concurrent spawns that together ask for more than the parent has, then exactly
//    the spawns that fit succeed (no overspend), tested with parallel threads.
#[test]
fn s3_concurrent_spawns_never_overspend() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("k.db");
    let parent = root(&Store::open(&path).unwrap(), "1.00");
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (barrier, path) = (barrier.clone(), path.clone());
            thread::spawn(move || {
                // One handle per thread, as separate processes would have.
                let store = Store::open(&path).unwrap();
                barrier.wait();
                spawn(&store, parent, Some(usd(3 * DOLLAR / 10)))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let won = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(won, 3, "0.30 fits three times into 1.00: {results:?}");
    for r in results.into_iter().filter(Result::is_err) {
        assert!(matches!(refusal(r), SpawnRefusal::Budget { .. }));
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.remaining(parent).unwrap(), usd(DOLLAR / 10));
    assert_eq!(store.children(parent).unwrap().len(), 3);
}

// 4. Given a child that spent $0.20 of $0.50 and then finished, then $0.30 returns to the parent.
#[test]
fn s4_a_finished_child_returns_its_unspent_budget() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let child = spawn(&store, parent, Some(usd(DOLLAR / 2))).unwrap().id;
    run(&store, child);
    spend(&store, child, DOLLAR / 5);
    finish(&store, child);
    assert_eq!(store.remaining(parent).unwrap(), usd(18 * DOLLAR / 10));
    // The child keeps exactly what it spent.
    assert_eq!(store.limit(child).unwrap(), usd(DOLLAR / 5));
    assert_eq!(store.remaining(child).unwrap(), usd(0));
}

#[test]
fn s4_a_child_cancelled_while_queued_returns_everything() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let child = spawn(&store, parent, Some(usd(DOLLAR / 2))).unwrap().id;
    store
        .transition(child, &RunStatus::Queued, RunStatus::Cancelled)
        .unwrap();
    assert_eq!(store.remaining(parent).unwrap(), usd(2 * DOLLAR));
}

#[test]
fn s4_a_child_that_overspent_returns_nothing_and_takes_nothing() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let child = spawn(&store, parent, Some(usd(DOLLAR / 2))).unwrap().id;
    run(&store, child);
    // A single model call can push a run past its limit before the engine suspends it.
    spend(&store, child, 6 * DOLLAR / 10);
    store
        .transition(child, &RunStatus::Running, RunStatus::Failed("over".into()))
        .unwrap();
    assert_eq!(store.remaining(parent).unwrap(), usd(3 * DOLLAR / 2));
}

#[test]
fn refunds_pass_through_a_finished_parent_to_the_next_live_one() {
    let store = Store::open_in_memory().unwrap();
    let root_id = root(&store, "4.00");
    let mid = spawn(&store, root_id, Some(usd(2 * DOLLAR))).unwrap().id;
    run(&store, mid);
    let leaf = spawn(&store, mid, Some(usd(DOLLAR))).unwrap().id;
    run(&store, leaf);
    // mid finishes first, having spent nothing: its own unspent $1 goes back to the root.
    finish(&store, mid);
    assert_eq!(store.remaining(root_id).unwrap(), usd(3 * DOLLAR));
    // leaf finishes later: its $1 passes through the finished mid to the root.
    finish(&store, leaf);
    assert_eq!(store.remaining(root_id).unwrap(), usd(4 * DOLLAR));
    assert_eq!(store.limit(mid).unwrap(), usd(0));
}

#[test]
fn a_finished_root_keeps_its_limit() {
    let store = Store::open_in_memory().unwrap();
    let id = root(&store, "2.00");
    spend(&store, id, DOLLAR);
    finish(&store, id);
    assert_eq!(store.limit(id).unwrap(), usd(2 * DOLLAR));
}

// 7, store side: `max_children` counts live and finished children.
#[test]
fn s7_max_children_counts_live_and_finished_children() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let spec = agent("child", "0.10");
    let mut limited = plan(&spec, None);
    limited.max_children = 2;
    let first = store
        .spawn_child(parent, limited.clone(), |_, _| Ok(None))
        .unwrap();
    store
        .transition(first.id, &RunStatus::Queued, RunStatus::Cancelled)
        .unwrap();
    store
        .spawn_child(parent, limited.clone(), |_, _| Ok(None))
        .unwrap();
    let third = store.spawn_child(parent, limited, |_, _| Ok(None));
    assert_eq!(refusal(third), SpawnRefusal::TooManyChildren(2));
    assert_eq!(store.children(parent).unwrap().len(), 2);
}

#[test]
fn max_tree_runs_counts_every_run_under_the_root() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let spec = agent("child", "0.10");
    let mut limited = plan(&spec, None);
    limited.max_tree_runs = 3;
    let a = store
        .spawn_child(parent, limited.clone(), |_, _| Ok(None))
        .unwrap();
    run(&store, a.id);
    store
        .spawn_child(a.id, limited.clone(), |_, _| Ok(None))
        .unwrap();
    // Root + a + a's child = 3 runs.
    let refused = store.spawn_child(parent, limited, |_, _| Ok(None));
    assert_eq!(refusal(refused), SpawnRefusal::TooManyRuns(3));
}

// 12. Given a parent in `Suspended(Budget)`, then spawning is refused until it is resumed.
#[test]
fn s12_a_suspended_parent_cannot_spawn_until_resumed() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let suspended = RunStatus::Suspended(kestrel_types::SuspendReason::Budget);
    store
        .transition(parent, &RunStatus::Running, suspended.clone())
        .unwrap();
    assert_eq!(
        refusal(spawn(&store, parent, Some(usd(DOLLAR / 10)))),
        SpawnRefusal::ParentNotRunning(parent, suspended.clone())
    );
    store
        .transition(parent, &suspended, RunStatus::Running)
        .unwrap();
    spawn(&store, parent, Some(usd(DOLLAR / 10))).unwrap();
}

// 14. Given a parent that does not exist or is already finished, then it fails and creates nothing.
#[test]
fn s14_a_missing_or_finished_parent_spawns_nothing() {
    let store = Store::open_in_memory().unwrap();
    let ghost = RunId::new();
    assert!(matches!(
        spawn(&store, ghost, Some(usd(1))),
        Err(StoreError::NotFound(id)) if id == ghost
    ));
    let parent = root(&store, "2.00");
    finish(&store, parent);
    assert_eq!(
        refusal(spawn(&store, parent, Some(usd(1)))),
        SpawnRefusal::ParentNotRunning(parent, RunStatus::Completed)
    );
    assert_eq!(run_count(&store), 1);
}

// Events: the child's `RunCreated` names the parent, extra child events follow it, and the parent
// gets `ChildSpawned`, all in the spawn's transaction.
#[test]
fn spawn_writes_run_created_child_events_and_child_spawned() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let spec = agent("child", "5.00");
    let mut p = plan(&spec, Some(usd(DOLLAR)));
    p.child_events = vec![EventKind::AgentLog {
        line: "dropped: tool:x".into(),
    }];
    let child = store.spawn_child(parent, p, |_, _| Ok(None)).unwrap().id;
    let kinds: Vec<EventKind> = store
        .events_since(child, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            EventKind::RunCreated {
                agent: "child".into(),
                parent: Some(parent)
            },
            EventKind::AgentLog {
                line: "dropped: tool:x".into()
            },
        ]
    );
    let last = store.events_since(parent, 0).unwrap().pop().unwrap().kind;
    assert_eq!(
        last,
        EventKind::ChildSpawned {
            child,
            agent: "child".into(),
            budget: usd(DOLLAR)
        }
    );
}

// The capability closure gets the child id and the carve, and its token is stored for the child.
#[test]
fn spawn_stores_the_childs_capability_token() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let spec = agent("child", "5.00");
    let mut seen = None;
    let child = store
        .spawn_child(parent, plan(&spec, None), |id, carve| {
            seen = Some((id, carve));
            Ok(Some(b"token".to_vec()))
        })
        .unwrap();
    assert_eq!(seen, Some((child.id, child.budget)));
    assert_eq!(store.run_cap(child.id).unwrap(), Some(b"token".to_vec()));
    assert_eq!(store.run_cap(parent).unwrap(), None);
}

#[test]
fn a_failing_capability_closure_writes_nothing() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let spec = agent("child", "5.00");
    let err = store
        .spawn_child(parent, plan(&spec, None), |_, _| {
            Err(StoreError::Capability("no key".into()))
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Capability(_)));
    assert_eq!(run_count(&store), 1);
    assert_eq!(store.remaining(parent).unwrap(), usd(2 * DOLLAR));
}

#[test]
fn run_caps_round_trip_and_need_a_run() {
    let store = Store::open_in_memory().unwrap();
    let id = root(&store, "1.00");
    store.set_run_cap(id, b"a").unwrap();
    store.set_run_cap(id, b"b").unwrap();
    assert_eq!(store.run_cap(id).unwrap(), Some(b"b".to_vec()));
    assert!(matches!(
        store.set_run_cap(RunId::new(), b"x"),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn limit_and_remaining_need_a_run() {
    let store = Store::open_in_memory().unwrap();
    assert!(matches!(
        store.limit(RunId::new()),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.remaining(RunId::new()),
        Err(StoreError::NotFound(_))
    ));
}

// 13, store side: the child, its link, its carve and its token survive reopening the file.
#[test]
fn s13_a_spawn_survives_reopening_the_store() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("k.db");
    let (parent, child) = {
        let store = Store::open(&path).unwrap();
        let parent = root(&store, "2.00");
        let spec = agent("child", "5.00");
        let child = store
            .spawn_child(parent, plan(&spec, Some(usd(DOLLAR / 2))), |_, _| {
                Ok(Some(b"tok".to_vec()))
            })
            .unwrap()
            .id;
        (parent, child)
    };
    let store = Store::open(&path).unwrap();
    assert_eq!(store.get_run(child).unwrap().unwrap().parent, Some(parent));
    assert_eq!(store.limit(child).unwrap(), usd(DOLLAR / 2));
    assert_eq!(store.remaining(parent).unwrap(), usd(3 * DOLLAR / 2));
    assert_eq!(store.run_cap(child).unwrap(), Some(b"tok".to_vec()));
}

// `kestrel budget` inside a tree: the money of a tree comes from its root. Raising a spawned run
// takes the difference from its parent; lowering one gives it back. A root's limit is its own
// share, set as given. No run is set below what it already spent.

#[test]
fn set_budget_on_a_child_takes_the_raise_from_its_parent() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let child = spawn(&store, parent, Some(usd(DOLLAR))).unwrap().id;
    assert_eq!(
        store.set_budget(child, usd(3 * DOLLAR / 2)).unwrap(),
        usd(3 * DOLLAR / 2)
    );
    assert_eq!(store.limit(child).unwrap(), usd(3 * DOLLAR / 2));
    assert_eq!(store.remaining(parent).unwrap(), usd(DOLLAR / 2));
}

#[test]
fn set_budget_on_a_child_is_refused_when_the_parent_cannot_cover_it() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "1.00");
    let child = spawn(&store, parent, Some(usd(9 * DOLLAR / 10)))
        .unwrap()
        .id;
    let err = store.set_budget(child, usd(2 * DOLLAR)).unwrap_err();
    assert!(
        matches!(err, StoreError::ParentBudget { asked, remaining }
            if asked == usd(11 * DOLLAR / 10) && remaining == usd(DOLLAR / 10)),
        "{err:?}"
    );
    assert_eq!(store.limit(child).unwrap(), usd(9 * DOLLAR / 10));
    assert_eq!(store.remaining(parent).unwrap(), usd(DOLLAR / 10));
}

// Lowering a run that already went over its limit leaves it where it is: lowering never takes
// money from the parent.
#[test]
fn set_budget_lowering_a_run_that_overspent_changes_nothing() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let child = spawn(&store, parent, Some(usd(DOLLAR / 2))).unwrap().id;
    run(&store, child);
    spend(&store, child, 55 * DOLLAR / 100);
    assert_eq!(
        store.set_budget(child, usd(DOLLAR / 10)).unwrap(),
        usd(DOLLAR / 2)
    );
    assert_eq!(store.remaining(parent).unwrap(), usd(3 * DOLLAR / 2));
}

#[test]
fn set_budget_lowering_a_child_gives_back_down_to_what_it_spent() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let child = spawn(&store, parent, Some(usd(DOLLAR))).unwrap().id;
    run(&store, child);
    spend(&store, child, 3 * DOLLAR / 10);
    // Asked for $0.10, but it already spent $0.30: it keeps $0.30 and gives back $0.70.
    assert_eq!(
        store.set_budget(child, usd(DOLLAR / 10)).unwrap(),
        usd(3 * DOLLAR / 10)
    );
    assert_eq!(store.remaining(parent).unwrap(), usd(17 * DOLLAR / 10));
}

#[test]
fn set_budget_on_a_root_sets_its_own_share_but_not_below_its_spend() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    spawn(&store, parent, Some(usd(DOLLAR))).unwrap();
    assert_eq!(
        store.set_budget(parent, usd(5 * DOLLAR)).unwrap(),
        usd(5 * DOLLAR)
    );
    assert_eq!(store.remaining(parent).unwrap(), usd(5 * DOLLAR));
    spend(&store, parent, DOLLAR);
    assert_eq!(store.set_budget(parent, usd(0)).unwrap(), usd(DOLLAR));
}

// The review's probe: raising a child and letting it finish never grows the tree.
#[test]
fn a_raised_child_that_finishes_leaves_the_tree_total_unchanged() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let child = spawn(&store, parent, Some(usd(DOLLAR / 2))).unwrap().id;
    run(&store, child);
    store.set_budget(child, usd(DOLLAR)).unwrap();
    spend(&store, child, DOLLAR / 2);
    finish(&store, child);
    let total = store.limit(parent).unwrap().0 + store.limit(child).unwrap().0;
    assert_eq!(total, 2 * DOLLAR);
    assert_eq!(store.remaining(parent).unwrap(), usd(3 * DOLLAR / 2));
}

#[test]
fn set_budget_needs_a_run() {
    let store = Store::open_in_memory().unwrap();
    assert!(matches!(
        store.set_budget(RunId::new(), usd(1)),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn live_children_counts_only_children_that_have_not_ended() {
    let store = Store::open_in_memory().unwrap();
    let parent = root(&store, "2.00");
    let a = spawn(&store, parent, Some(usd(DOLLAR / 10))).unwrap().id;
    let b = spawn(&store, parent, Some(usd(DOLLAR / 10))).unwrap().id;
    let c = spawn(&store, parent, Some(usd(DOLLAR / 10))).unwrap().id;
    run(&store, b);
    run(&store, c);
    assert_eq!(store.live_children(parent).unwrap(), 3);
    store
        .transition(a, &RunStatus::Queued, RunStatus::Cancelled)
        .unwrap();
    store
        .transition(b, &RunStatus::Running, RunStatus::Failed("x".into()))
        .unwrap();
    finish(&store, c);
    assert_eq!(store.live_children(parent).unwrap(), 0);
}

#[derive(Debug, Clone)]
enum Op {
    /// Spawn under the live run at this index (modulo), asking this many thousandths of what
    /// it has left (or the default carve for 0).
    Spawn(usize, u16),
    /// Spawn under the live run at this index and cancel the child while it is still queued.
    SpawnAndCancel(usize, u16),
    /// The live run at this index spends what it has left, plus this many micro-dollars over:
    /// one gateway call can go past a run's limit before the engine stops it.
    Spend(usize, u16),
    /// The live run at this index finishes.
    Finish(usize),
    /// The live run at this index fails.
    Fail(usize),
    /// A human sets the budget of the run at this index to this many thousandths of $2.
    SetBudget(usize, u16),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (any::<usize>(), 0u16..=1000).prop_map(|(i, f)| Op::Spawn(i, f)),
        (any::<usize>(), 0u16..=1000).prop_map(|(i, f)| Op::SpawnAndCancel(i, f)),
        (any::<usize>(), 0u16..=50).prop_map(|(i, over)| Op::Spend(i, over)),
        any::<usize>().prop_map(Op::Finish),
        any::<usize>().prop_map(Op::Fail),
        (any::<usize>(), 0u16..=2000).prop_map(|(i, f)| Op::SetBudget(i, f)),
    ]
}

fn ask(store: &Store, parent: RunId, f: u16) -> Option<MicroUsd> {
    let left = store.remaining(parent).unwrap().0;
    (f > 0).then(|| usd(left * i64::from(f) / 1000))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    // 5. Given a tree with a $2 root, when every run spends its full budget, then the total spend
    //    is at most $2.00 plus what single calls went over. Checked after every step: the limits
    //    of all runs add up to the root budget (which only a human change to the root's own
    //    budget moves), and no run spends more than its limit plus its own overshoot. Steps mix
    //    spawns, cancels of queued children, overspend, finish, fail and `kestrel budget`.
    #[test]
    fn s5_a_tree_never_spends_more_than_the_root_budget(ops in prop::collection::vec(op(), 1..48)) {
        let store = Store::open_in_memory().unwrap();
        let root_id = root(&store, "2.00");
        let mut all = vec![root_id];
        let mut live = vec![root_id];
        let mut budget = 2 * DOLLAR;
        let mut parents: std::collections::HashMap<RunId, RunId> = Default::default();
        let mut over: std::collections::HashMap<RunId, i64> = Default::default();
        for op in ops {
            match op {
                Op::Spawn(i, f) => {
                    let parent = live[i % live.len()];
                    if let Ok(child) = spawn(&store, parent, ask(&store, parent, f)) {
                        parents.insert(child.id, parent);
                        run(&store, child.id);
                        all.push(child.id);
                        live.push(child.id);
                    }
                }
                Op::SpawnAndCancel(i, f) => {
                    let parent = live[i % live.len()];
                    if let Ok(child) = spawn(&store, parent, ask(&store, parent, f)) {
                        parents.insert(child.id, parent);
                        all.push(child.id);
                        store
                            .transition(child.id, &RunStatus::Queued, RunStatus::Cancelled)
                            .unwrap();
                    }
                }
                Op::Spend(i, extra) => {
                    let r = live[i % live.len()];
                    let left = store.remaining(r).unwrap().0;
                    if left > 0 {
                        spend(&store, r, left + i64::from(extra));
                        *over.entry(r).or_default() += i64::from(extra);
                    }
                }
                Op::Finish(i) | Op::Fail(i) if live.len() > 1 => {
                    let r = live.remove(1 + i % (live.len() - 1));
                    let end = if matches!(op, Op::Finish(_)) {
                        RunStatus::Completed
                    } else {
                        RunStatus::Failed("x".into())
                    };
                    store.transition(r, &RunStatus::Running, end).unwrap();
                }
                Op::Finish(_) | Op::Fail(_) => {}
                Op::SetBudget(i, f) => {
                    let r = all[i % all.len()];
                    let before = store.limit(r).unwrap().0;
                    let parent = parents.get(&r).copied();
                    let theirs = parent.map(|p| store.limit(p).unwrap().0);
                    let asked = usd(2 * DOLLAR * i64::from(f) / 1000);
                    match store.set_budget(r, asked) {
                        Ok(set) if r == root_id => budget += set.0 - before,
                        Ok(set) => {
                            // A raise comes out of the parent, exactly.
                            if set.0 > before {
                                let now = store.limit(parent.unwrap()).unwrap().0;
                                prop_assert_eq!(now, theirs.unwrap() - (set.0 - before));
                            }
                            // Never more than asked when lowering.
                            if asked.0 < before {
                                prop_assert!(set.0 <= before);
                            }
                        }
                        Err(StoreError::ParentBudget { .. }) => {
                            prop_assert!(parent.is_some());
                            prop_assert_eq!(store.limit(r).unwrap().0, before);
                            prop_assert_eq!(parent.map(|p| store.limit(p).unwrap().0), theirs);
                        }
                        Err(other) => prop_assert!(false, "unexpected {other:?}"),
                    }
                }
            }
            let limits: i64 = all.iter().map(|r| store.limit(*r).unwrap().0).sum();
            prop_assert_eq!(limits, budget);
            for r in &all {
                let allowed = store.limit(*r).unwrap().0 + over.get(r).copied().unwrap_or(0);
                prop_assert!(store.spent(*r).unwrap().0 <= allowed);
            }
        }
        for r in live.iter().rev() {
            let left = store.remaining(*r).unwrap().0;
            if left > 0 {
                spend(&store, *r, left);
            }
        }
        let spent: i64 = all.iter().map(|r| store.spent(*r).unwrap().0).sum();
        let overshoot: i64 = over.values().sum();
        prop_assert!(spent <= budget + overshoot, "tree spent {spent} of {budget} (+{overshoot})");
    }
}
