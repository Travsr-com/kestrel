//! Issue #25: `Store::create_run_with_event` writes the run row, its spec, its token and the
//! `RunCreated` event in one transaction, so a failed event insert leaves no run behind.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_store::{RunFilter, Store};
use kestrel_types::{AgentSpec, EventKind, RunStatus};

fn agent() -> AgentSpec {
    let yaml = "name: a\nruntime: opencode\nrole: r\nmodel_policy: { default: tier:cheap }\n\
        sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }\n\
        budget: { usd: 1.00, wall_clock: 10m }\n";
    AgentSpec::from_yaml(yaml).expect("fixture parses")
}

#[test]
fn one_call_creates_the_run_its_spec_its_token_and_a_run_created_event_at_seq_1() {
    let store = Store::open_in_memory().expect("open");
    let id = store
        .create_run_with_event(&agent(), "task")
        .expect("create");
    let run = store.get_run(id).expect("get").expect("row");
    assert_eq!(run.status, RunStatus::Queued);
    assert_eq!(run.agent, "a");
    assert!(store.run_spec(id).expect("spec").is_some());
    assert!(store.run_token(id).expect("token").is_some());
    let events = store.events_since(id, 0).expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[0].kind, EventKind::RunCreated { agent: "a".into() });
}

#[test]
fn the_next_appended_event_follows_the_run_created_event() {
    let store = Store::open_in_memory().expect("open");
    let id = store
        .create_run_with_event(&agent(), "task")
        .expect("create");
    let next = store
        .append(id, EventKind::AgentLog { line: "x".into() })
        .expect("append");
    assert_eq!(next.seq, 2);
}

#[test]
fn a_failing_event_insert_leaves_no_run_row_spec_or_token() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("k.db");
    let store = Store::open(&path).expect("open");
    // A second connection breaks the events table, so the event insert fails.
    let other = rusqlite::Connection::open(&path).expect("second connection");
    other
        .execute_batch("DROP TABLE events;")
        .expect("drop events");
    drop(other);

    let result = store.create_run_with_event(&agent(), "task");
    assert!(result.is_err(), "the event insert must fail the call");

    assert!(
        store
            .list_runs(RunFilter::default())
            .expect("list")
            .is_empty()
    );
    let raw = rusqlite::Connection::open(&path).expect("raw");
    for table in ["runs", "run_specs", "run_tokens"] {
        let n: i64 = raw
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 0, "{table} must be empty after the rollback");
    }
}
