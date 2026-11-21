//! Issue #41: the events of every run on the global cursor, optionally only some kinds, and the
//! cursor of the newest event (where a client starts when it wants only what comes next).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_store::Store;
use kestrel_types::{AgentSpec, ApprovalId, EventKind, RunId, RunStatus};

fn agent(name: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: opencode\nrole: r\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 1.00, wall_clock: 10m }}\n"
    );
    AgentSpec::from_yaml(&yaml).unwrap()
}

fn log(store: &Store, run: RunId, line: &str) {
    store
        .append(run, EventKind::AgentLog { line: line.into() })
        .unwrap();
}

fn ask(store: &Store, run: RunId, action: &str) -> ApprovalId {
    let id = ApprovalId::new();
    store
        .append(
            run,
            EventKind::ApprovalRequested {
                id,
                action: action.into(),
                question: false,
            },
        )
        .unwrap();
    id
}

#[test]
fn every_run_in_commit_order_on_the_global_cursor() {
    let store = Store::open_in_memory().unwrap();
    let a = store.create_run(&agent("a"), "x").unwrap();
    let b = store.create_run(&agent("b"), "y").unwrap();
    let kid = store.create_child_run(a, &agent("k"), "z").unwrap();
    for run in [a, b, kid, b] {
        log(&store, run, "l");
    }
    let all = store.events_after_page(0, &[], 100).unwrap();
    assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
    let runs: Vec<RunId> = all.iter().map(|(_, e)| e.run_id).collect();
    assert_eq!(runs, [a, b, kid, b]);
    // It matches the tree query for the same events.
    let tree: Vec<u64> = store
        .events_since_global(a, 0)
        .unwrap()
        .iter()
        .map(|(c, _)| *c)
        .collect();
    let mine: Vec<u64> = all
        .iter()
        .filter(|(_, e)| e.run_id == a || e.run_id == kid)
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(tree, mine);
}

#[test]
fn only_the_kinds_asked_for_and_pages_continue_from_the_cursor() {
    let store = Store::open_in_memory().unwrap();
    let a = store.create_run(&agent("a"), "x").unwrap();
    let b = store.create_run(&agent("b"), "y").unwrap();
    let first = ask(&store, a, "git push");
    log(&store, a, "noise");
    store
        .transition(b, &RunStatus::Queued, RunStatus::Cancelled)
        .unwrap();
    let second = ask(&store, b, "rm build");
    let kinds = ["approval_requested", "status_changed"];
    let page = store.events_after_page(0, &kinds, 2).unwrap();
    assert_eq!(page.len(), 2);
    assert!(matches!(page[0].1.kind, EventKind::ApprovalRequested { id, .. } if id == first));
    assert!(matches!(page[1].1.kind, EventKind::StatusChanged { .. }));
    let rest = store.events_after_page(page[1].0, &kinds, 2).unwrap();
    assert_eq!(rest.len(), 1);
    assert!(matches!(rest[0].1.kind, EventKind::ApprovalRequested { id, .. } if id == second));
    assert!(
        store
            .events_after_page(rest[0].0, &kinds, 2)
            .unwrap()
            .is_empty()
    );
    // A kind no event has matches nothing.
    assert!(
        store
            .events_after_page(0, &["no_such_kind"], 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_latest_cursor_is_where_only_new_events_start() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.latest_cursor().unwrap(), 0);
    let a = store.create_run(&agent("a"), "x").unwrap();
    log(&store, a, "old");
    let latest = store.latest_cursor().unwrap();
    let all = store.events_after_page(0, &[], 100).unwrap();
    assert_eq!(all.last().map(|(c, _)| *c), Some(latest));
    assert!(
        store
            .events_after_page(latest, &[], 100)
            .unwrap()
            .is_empty()
    );
    log(&store, a, "new");
    let next = store.events_after_page(latest, &[], 100).unwrap();
    assert_eq!(next.len(), 1);
    assert!(matches!(&next[0].1.kind, EventKind::AgentLog { line } if line == "new"));
}

#[test]
fn a_question_is_marked_as_one_in_the_approvals() {
    let store = Store::open_in_memory().unwrap();
    let a = store.create_run(&agent("a"), "x").unwrap();
    let action = ask(&store, a, "git push");
    let question = ApprovalId::new();
    store
        .append(
            a,
            EventKind::ApprovalRequested {
                id: question,
                action: "Which branch should I use?".into(),
                question: true,
            },
        )
        .unwrap();
    let records = store.approvals().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!((records[0].id, records[0].question), (action, false));
    assert_eq!((records[1].id, records[1].question), (question, true));
}
