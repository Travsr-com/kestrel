//! Issue #37: paged event reads, so an event socket never loads a whole history at once.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_store::Store;
use kestrel_types::{AgentSpec, EventKind};

fn agent() -> AgentSpec {
    AgentSpec::from_yaml(
        "name: a\nruntime: opencode\nrole: r\n\
         sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }\n\
         budget: { usd: 1.00, wall_clock: 10m }\n",
    )
    .unwrap()
}

fn log(store: &Store, run: kestrel_types::RunId, n: usize) {
    for i in 0..n {
        store
            .append(
                run,
                EventKind::AgentLog {
                    line: format!("{i}"),
                },
            )
            .unwrap();
    }
}

#[test]
fn a_run_page_holds_at_most_limit_events_from_the_cursor() {
    let store = Store::open_in_memory().unwrap();
    let run = store.create_run(&agent(), "x").unwrap();
    log(&store, run, 10);
    let seqs = |after, limit| -> Vec<u64> {
        store
            .events_page(run, after, limit)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect()
    };
    assert_eq!(seqs(0, 4), [1, 2, 3, 4]);
    assert_eq!(seqs(4, 4), [5, 6, 7, 8]);
    assert_eq!(seqs(8, 4), [9, 10]);
    assert!(seqs(10, 4).is_empty());
    assert!(seqs(0, 0).is_empty());
}

#[test]
fn a_tree_page_holds_at_most_limit_events_in_commit_order() {
    let store = Store::open_in_memory().unwrap();
    let root = store.create_run(&agent(), "x").unwrap();
    let child = store.create_child_run(root, &agent(), "y").unwrap();
    let other = store.create_run(&agent(), "z").unwrap();
    for _ in 0..3 {
        log(&store, root, 1);
        log(&store, other, 1);
        log(&store, child, 1);
    }
    let first = store.events_since_global_page(root, 0, 4).unwrap();
    assert_eq!(first.len(), 4);
    assert!(first.iter().all(|(_, e)| e.run_id != other));
    let cursor = first.last().unwrap().0;
    let rest = store.events_since_global_page(root, cursor, 100).unwrap();
    assert_eq!(rest.len(), 2);
    assert!(rest[0].0 > cursor);
    let all = store.events_since_global(root, 0).unwrap();
    let paged: Vec<u64> = first.iter().chain(&rest).map(|(c, _)| *c).collect();
    assert_eq!(paged, all.iter().map(|(c, _)| *c).collect::<Vec<_>>());
}
