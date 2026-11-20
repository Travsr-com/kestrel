//! Issue #32, scenarios 4 to 7 and 10: the run tree (`create_child_run`, `children`, `subtree`,
//! `ancestors`).

// Test helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use kestrel_store::{RunFilter, RunRow, Store, StoreError};
use kestrel_types::{AgentSpec, EventKind, RunId, RunStatus};
use tempfile::TempDir;

fn agent(name: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: opencode\nrole: r\nmodel_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 1.00, wall_clock: 10m }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture parses")
}

fn root(store: &Store, name: &str) -> RunId {
    store.create_run(&agent(name), name).expect("create_run")
}

/// A child of `parent` whose agent name and input are both `name`.
fn child(store: &Store, parent: RunId, name: &str) -> RunId {
    store
        .create_child_run(parent, &agent(name), name)
        .expect("create_child_run")
}

fn row(store: &Store, id: RunId) -> RunRow {
    store.get_run(id).expect("get_run").expect("run exists")
}

fn ids(rows: &[RunRow]) -> Vec<RunId> {
    rows.iter().map(|r| r.id).collect()
}

fn id_set(rows: &[RunRow]) -> HashSet<RunId> {
    rows.iter().map(|r| r.id).collect()
}

/// Checks the order `subtree(top)` promises: every row's parent is `top` or an earlier row, and
/// no run is listed twice.
fn assert_parents_first(top: RunId, rows: &[RunRow]) {
    let mut seen = HashSet::from([top]);
    for r in rows {
        let parent = r.parent.expect("a descendant has a parent");
        assert!(
            seen.contains(&parent),
            "run {} is listed before its parent {parent}",
            r.id
        );
        assert!(seen.insert(r.id), "run {} is listed twice", r.id);
    }
}

/// Row counts of every table a run creation could touch, read with a raw connection.
fn counts(path: &Path) -> Vec<(&'static str, i64)> {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    ["runs", "run_specs", "run_tokens", "run_caps", "events"]
        .into_iter()
        .map(|table| {
            let n = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .expect("count");
            (table, n)
        })
        .collect()
}

// `create_run` keeps its signature and makes a root: no parent, its own root, depth 0.
#[test]
fn create_run_makes_a_root_at_depth_0() {
    let store = Store::open_in_memory().expect("open");
    let a = root(&store, "a");
    let b = root(&store, "b");
    for id in [a, b] {
        let r = row(&store, id);
        assert_eq!((r.parent, r.root, r.depth), (None, id, 0));
    }
}

// 4. Given a root with two children and one grandchild, when `subtree(root)` is called, then it
//    returns all three descendants, each parent before its children, and `children(root)` returns
//    only the two direct children.
#[test]
fn s4_subtree_returns_all_descendants_parents_first_and_children_only_the_direct_ones() {
    let store = Store::open_in_memory().expect("open");
    let top = root(&store, "root");
    let c1 = child(&store, top, "child-one");
    let c2 = child(&store, top, "child-two");
    let g = child(&store, c1, "grandchild");
    // A second tree, which must never show up in the first.
    let other = root(&store, "other");
    let other_child = child(&store, other, "other-child");

    let subtree = store.subtree(top).expect("subtree");
    assert_eq!(subtree.len(), 3, "{subtree:?}");
    assert_eq!(id_set(&subtree), HashSet::from([c1, c2, g]));
    assert_parents_first(top, &subtree);
    for r in &subtree {
        assert_eq!(r, &row(&store, r.id), "subtree rows are full run rows");
        assert_eq!(r.root, top);
    }

    let children = store.children(top).expect("children");
    assert_eq!(
        ids(&children),
        vec![c1, c2],
        "direct children, oldest first"
    );
    for r in &children {
        assert_eq!(r, &row(&store, r.id), "children rows are full run rows");
        assert_eq!((r.parent, r.root, r.depth), (Some(top), top, 1));
    }

    let grandchild = row(&store, g);
    assert_eq!(
        (grandchild.parent, grandchild.root, grandchild.depth),
        (Some(c1), top, 2)
    );
    assert_eq!(grandchild.agent, "grandchild");
    assert_eq!(grandchild.input, "grandchild");

    // The same calls work below the root.
    assert_eq!(ids(&store.children(c1).expect("children")), vec![g]);
    assert_eq!(ids(&store.subtree(c1).expect("subtree")), vec![g]);
    assert!(store.children(c2).expect("children").is_empty());
    assert!(store.children(g).expect("children").is_empty());
    assert!(store.subtree(g).expect("subtree").is_empty());

    // The other tree is separate.
    assert_eq!(
        ids(&store.children(other).expect("children")),
        vec![other_child]
    );
    assert_eq!(
        ids(&store.subtree(other).expect("subtree")),
        vec![other_child]
    );

    // An unknown run has no children and no descendants.
    let ghost = RunId::new();
    assert!(store.children(ghost).expect("children").is_empty());
    assert!(store.subtree(ghost).expect("subtree").is_empty());
}

// 4. The order holds in a wider, deeper tree whose branches were created out of order.
#[test]
fn s4_subtree_puts_every_parent_before_its_children_in_a_wider_tree() {
    let store = Store::open_in_memory().expect("open");
    let top = root(&store, "root");
    let a = child(&store, top, "a");
    let b = child(&store, top, "b");
    // b gets its child before a does, and a's branch grows last.
    let b1 = child(&store, b, "b1");
    let a1 = child(&store, a, "a1");
    let b1x = child(&store, b1, "b1x");
    let a2 = child(&store, a, "a2");
    let a1x = child(&store, a1, "a1x");
    let a1xy = child(&store, a1x, "a1xy");

    let subtree = store.subtree(top).expect("subtree");
    assert_eq!(
        id_set(&subtree),
        HashSet::from([a, b, b1, a1, b1x, a2, a1x, a1xy])
    );
    assert_eq!(subtree.len(), 8, "{subtree:?}");
    assert_parents_first(top, &subtree);

    let below_a = store.subtree(a).expect("subtree");
    assert_eq!(id_set(&below_a), HashSet::from([a1, a2, a1x, a1xy]));
    assert_eq!(below_a.len(), 4, "{below_a:?}");
    assert_parents_first(a, &below_a);

    assert_eq!(ids(&store.children(a).expect("children")), vec![a1, a2]);
    assert_eq!(ids(&store.children(top).expect("children")), vec![a, b]);
    assert_eq!(row(&store, a1xy).depth, 4);

    // `list_runs` returns the tree fields too.
    let listed = store.list_runs(RunFilter::default()).expect("list_runs");
    assert_eq!(listed.len(), 9);
    for r in &listed {
        assert_eq!(r, &row(&store, r.id));
        assert_eq!(r.root, top);
    }
}

// 5. Given a grandchild, when `ancestors` is called, then it returns parent then root, and for a
//    root it returns an empty list.
#[test]
fn s5_ancestors_returns_parent_then_root_and_nothing_for_a_root() {
    let store = Store::open_in_memory().expect("open");
    let top = root(&store, "root");
    let c1 = child(&store, top, "child-one");
    let c2 = child(&store, top, "child-two");
    let g = child(&store, c1, "grandchild");

    let above = store.ancestors(g).expect("ancestors");
    assert_eq!(ids(&above), vec![c1, top], "parent first, root last");
    assert_eq!(above, vec![row(&store, c1), row(&store, top)]);

    assert_eq!(ids(&store.ancestors(c1).expect("ancestors")), vec![top]);
    assert_eq!(ids(&store.ancestors(c2).expect("ancestors")), vec![top]);
    assert!(store.ancestors(top).expect("ancestors").is_empty());
    assert!(store.ancestors(RunId::new()).expect("ancestors").is_empty());
}

// 6. Given `create_child_run` with an unknown parent, when it is called, then it fails and
//    creates nothing.
#[test]
fn s6_create_child_run_with_unknown_parent_fails_and_creates_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kestrel.db");
    let store = Store::open(&path).expect("open");
    let existing = root(&store, "existing");
    let before = counts(&path);
    let listed = store.list_runs(RunFilter::default()).expect("list_runs");

    let ghost = RunId::new();
    match store.create_child_run(ghost, &agent("orphan"), "x") {
        Err(StoreError::NotFound(id)) => assert_eq!(id, ghost),
        other => panic!("expected NotFound, got {other:?}"),
    }

    assert_eq!(counts(&path), before, "a failed create wrote rows");
    assert_eq!(
        store.list_runs(RunFilter::default()).expect("list_runs"),
        listed
    );
    assert!(store.children(ghost).expect("children").is_empty());
    assert!(store.children(existing).expect("children").is_empty());
    assert_eq!(store.get_run(ghost).expect("get_run"), None);
}

// 7. Given a parent at depth 2, when a child is created, then the child has depth 3 and the same
//    root.
#[test]
fn s7_child_of_a_depth_2_parent_has_depth_3_and_the_same_root() {
    let store = Store::open_in_memory().expect("open");
    let top = root(&store, "root");
    let d1 = child(&store, top, "d1");
    let d2 = child(&store, d1, "d2");
    let parent = row(&store, d2);
    assert_eq!((parent.root, parent.depth), (top, 2));

    let d3 = child(&store, d2, "d3");
    let r = row(&store, d3);
    assert_eq!(r.depth, 3);
    assert_eq!(r.root, top);
    assert_eq!(r.parent, Some(d2));

    assert_eq!(
        ids(&store.ancestors(d3).expect("ancestors")),
        vec![d2, d1, top]
    );
    assert_eq!(ids(&store.subtree(top).expect("subtree")), vec![d1, d2, d3]);
    let depths: Vec<u32> = [top, d1, d2, d3]
        .into_iter()
        .map(|id| row(&store, id).depth)
        .collect();
    assert_eq!(depths, vec![0, 1, 2, 3]);
}

// `create_child_run` is `create_run` plus the tree fields: a Queued run with its spec and a fresh
// token, and no event (the child's first appended event gets seq 1).
#[test]
fn child_run_is_queued_with_its_spec_a_fresh_token_and_no_events() {
    let store = Store::open_in_memory().expect("open");
    let top = root(&store, "root");
    let spec = agent("worker");
    let id = store
        .create_child_run(top, &spec, "do the thing")
        .expect("create_child_run");
    assert_ne!(id, top);

    let r = row(&store, id);
    assert_eq!(r.agent, "worker");
    assert_eq!(r.input, "do the thing");
    assert_eq!(r.status, RunStatus::Queued);

    assert_eq!(store.run_spec(id).expect("run_spec"), Some(spec));
    assert_eq!(
        store.run_spec(top).expect("run_spec"),
        Some(agent("root")),
        "the parent keeps its own spec"
    );

    let token = store.run_token(id).expect("run_token").expect("has token");
    assert_eq!(token.len(), 32, "128 bits as hex: {token}");
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
    assert_ne!(
        Some(&token),
        store.run_token(top).expect("run_token").as_ref()
    );
    assert_eq!(store.run_by_token(&token).expect("run_by_token"), Some(id));

    assert!(store.events_since(id, 0).expect("events_since").is_empty());
    assert!(store.events_since(top, 0).expect("events_since").is_empty());
    let first = store
        .append(id, EventKind::AgentMessage { text: "hi".into() })
        .expect("append");
    assert_eq!(first.seq, 1);

    // A child is a normal run: it moves through statuses on its own.
    store
        .transition(id, &RunStatus::Queued, RunStatus::Cancelled)
        .expect("transition");
    assert_eq!(row(&store, id).status, RunStatus::Cancelled);
    assert_eq!(row(&store, top).status, RunStatus::Queued);
}

// The tree is stored, not kept in memory: it is the same after the file is reopened.
#[test]
fn run_tree_survives_reopening_the_file() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kestrel.db");
    let (top, c, g, rows) = {
        let store = Store::open(&path).expect("open");
        let top = root(&store, "root");
        let c = child(&store, top, "child");
        let g = child(&store, c, "grandchild");
        (
            top,
            c,
            g,
            [row(&store, top), row(&store, c), row(&store, g)],
        )
    };

    let store = Store::open(&path).expect("reopen");
    assert_eq!([row(&store, top), row(&store, c), row(&store, g)], rows);
    assert_eq!(ids(&store.subtree(top).expect("subtree")), vec![c, g]);
    assert_eq!(ids(&store.ancestors(g).expect("ancestors")), vec![c, top]);

    // A second handle (another process) sees children made through the first.
    let other = Store::open(&path).expect("second handle");
    let late = child(&store, c, "late");
    assert_eq!(ids(&other.children(c).expect("children")), vec![g, late]);
    assert_eq!(row(&other, late).depth, 2);
}

/// How often scenario 10 repeats the race, each time on a new parent.
const ROUNDS: usize = 20;

/// Scenario 10's "then": both creates produced a child, `children` lists exactly those two, and
/// each has the parent's root and the parent's depth plus one.
fn assert_both_children(store: &Store, parent: RunId, made: &[RunId]) {
    let parent_row = row(store, parent);
    assert_eq!(made.len(), 2);
    assert_ne!(made[0], made[1], "two creates returned the same id");

    let children = store.children(parent).expect("children");
    assert_eq!(children.len(), 2, "lost or duplicated child: {children:?}");
    assert_eq!(
        id_set(&children),
        made.iter().copied().collect::<HashSet<RunId>>()
    );
    for c in &children {
        assert_eq!(c.parent, Some(parent));
        assert_eq!(c.root, parent_row.root);
        assert_eq!(c.depth, parent_row.depth + 1);
        assert_eq!(c.status, RunStatus::Queued);
        assert!(store.run_spec(c.id).expect("run_spec").is_some());
    }
    let tokens: HashSet<String> = made
        .iter()
        .map(|id| store.run_token(*id).expect("run_token").expect("has token"))
        .collect();
    assert_eq!(tokens.len(), 2, "each child has its own token");
}

// 10. Given two threads creating children of the same parent at once, then both succeed and
//     `children` lists both (no lost write, no duplicate depth bug). One shared handle.
#[test]
fn s10_two_threads_sharing_a_handle_both_create_a_child_of_the_same_parent() {
    let dir = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(dir.path().join("kestrel.db")).expect("open"));
    let top = root(&store, "root");

    for round in 0..ROUNDS {
        // The parent is itself a child, so "depth + 1" is 2 and cannot pass by default.
        let parent = child(&store, top, &format!("parent-{round}"));
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let spec = agent(&format!("kid-{t}"));
                    barrier.wait();
                    store.create_child_run(parent, &spec, "x")
                })
            })
            .collect();
        let made: Vec<RunId> = threads
            .into_iter()
            .map(|t| {
                t.join()
                    .expect("thread panicked")
                    .expect("create_child_run")
            })
            .collect();

        assert_both_children(&store, parent, &made);
        assert_eq!(row(&store, made[0]).depth, 2);
    }

    let subtree = store.subtree(top).expect("subtree");
    assert_eq!(subtree.len(), ROUNDS * 3);
    assert_parents_first(top, &subtree);
    assert_eq!(store.children(top).expect("children").len(), ROUNDS);
}

// 10. Same scenario, each thread with its own `Store` handle on the same file (like separate
//     processes sharing the database).
#[test]
fn s10_two_threads_with_separate_handles_both_create_a_child_of_the_same_parent() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kestrel.db");
    let store = Store::open(&path).expect("open");
    let top = root(&store, "root");

    for round in 0..ROUNDS {
        let parent = child(&store, top, &format!("parent-{round}"));
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let own = Store::open(&path).expect("open per thread");
                    let spec = agent(&format!("kid-{t}"));
                    barrier.wait();
                    own.create_child_run(parent, &spec, "x")
                })
            })
            .collect();
        let made: Vec<RunId> = threads
            .into_iter()
            .map(|t| {
                t.join()
                    .expect("thread panicked")
                    .expect("create_child_run")
            })
            .collect();

        assert_both_children(&store, parent, &made);
        assert_eq!(row(&store, made[0]).depth, 2);
    }

    let subtree = store.subtree(top).expect("subtree");
    assert_eq!(subtree.len(), ROUNDS * 3);
    assert_parents_first(top, &subtree);
    assert_eq!(store.children(top).expect("children").len(), ROUNDS);
}
