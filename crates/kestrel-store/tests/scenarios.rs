//! Scenarios 1 to 12 from issue #2: SQLite event log, run store and CLI control channel.

// Test helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use kestrel_store::{ControlCommand, SCHEMA_VERSION, Store, StoreError};
use kestrel_types::{
    AgentSpec, ApprovalId, EventKind, MicroUsd, ModelCallRecord, RunId, RunStatus,
};
use tempfile::TempDir;

const AGENT_YAML: &str = r#"
name: store-tester
runtime: claude-code
role: tester
model_policy: { default: tier:mid }
sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: git-worktree }
budget: { usd: 1.00, wall_clock: 10m }
"#;

fn agent() -> AgentSpec {
    AgentSpec::from_yaml(AGENT_YAML).expect("fixture agent spec parses")
}

fn db_path(dir: &TempDir) -> PathBuf {
    dir.path().join("kestrel.db")
}

fn msg(i: usize) -> EventKind {
    EventKind::AgentMessage {
        text: format!("m{i}"),
    }
}

fn seqs(store: &Store, run: RunId) -> Vec<u64> {
    store
        .events_since(run, 0)
        .expect("events_since")
        .iter()
        .map(|e| e.seq)
        .collect()
}

fn call(run: RunId, cost: Option<i64>) -> ModelCallRecord {
    ModelCallRecord {
        run_id: run,
        provider: "anthropic".into(),
        model: "test-model".into(),
        tokens_in: 10,
        tokens_out: 20,
        tokens_cache_read: 0,
        tokens_cache_write: 0,
        cost: cost.map(MicroUsd),
        latency_ms: 5,
        status: 200,
    }
}

/// Walks a fresh run through legal transitions to `target`.
fn drive_to(store: &Store, run: RunId, target: &RunStatus) {
    let path = [
        RunStatus::Queued,
        RunStatus::Provisioning,
        RunStatus::Running,
    ];
    let mut cur = RunStatus::Queued;
    for next in path.iter().skip(1) {
        if &cur == target {
            return;
        }
        store
            .transition(run, &cur, next.clone())
            .expect("legal transition");
        cur = next.clone();
    }
    if &cur != target {
        store
            .transition(run, &cur, target.clone())
            .expect("legal transition to target");
    }
}

fn status_of(store: &Store, run: RunId) -> RunStatus {
    store
        .get_run(run)
        .expect("get_run")
        .expect("run exists")
        .status
}

/// `PRAGMA user_version` and the full `sqlite_master` dump, read with a raw connection.
type SchemaRow = (String, String, String, Option<String>);

fn schema_snapshot(path: &Path) -> (i64, Vec<SchemaRow>) {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version");
    let mut stmt = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");
    (version, rows)
}

// 1. Given a new store, when 3 events are appended to one run, then their seq values are 1, 2, 3.
#[test]
fn s1_three_appends_get_seq_1_2_3() {
    let store = Store::open_in_memory().expect("open");
    let run = store.create_run(&agent(), "hello").expect("create_run");

    let got: Vec<u64> = (0..3)
        .map(|i| {
            let ev = store.append(run, msg(i)).expect("append");
            assert_eq!(ev.run_id, run);
            assert_eq!(ev.kind, msg(i));
            ev.seq
        })
        .collect();

    assert_eq!(got, vec![1, 2, 3]);
    assert_eq!(seqs(&store, run), vec![1, 2, 3]);
}

fn assert_800_gapless(store: &Store, run: RunId) {
    let events = store.events_since(run, 0).expect("events_since");
    assert_eq!(events.len(), 800, "exactly 800 events");
    let got: Vec<u64> = events.iter().map(|e| e.seq).collect();
    let want: Vec<u64> = (1..=800).collect();
    assert_eq!(got, want, "seqs 1..=800, no gaps or duplicates");
    let mut texts: Vec<&str> = events
        .iter()
        .map(|e| match &e.kind {
            EventKind::AgentMessage { text } => text.as_str(),
            other => panic!("unexpected event {other:?}"),
        })
        .collect();
    texts.sort_unstable();
    texts.dedup();
    assert_eq!(texts.len(), 800, "every appended payload stored once");
}

// 2. Given 8 tasks appending 100 events each to the same run concurrently, then there are exactly
//    800 events, seqs 1..=800 with no gaps or duplicates. (One shared handle.)
#[test]
fn s2_concurrent_appends_shared_handle_are_gapless() {
    let dir = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(db_path(&dir)).expect("open"));
    let run = store.create_run(&agent(), "x").expect("create_run");
    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|t| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..100 {
                    store.append(run, msg(t * 100 + i)).expect("append");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("task panicked");
    }

    assert_800_gapless(&store, run);
}

// 2. Same scenario, each task with its own `Store` handle on the same file (like separate
//    processes sharing the database).
#[test]
fn s2_concurrent_appends_separate_handles_are_gapless() {
    let dir = TempDir::new().expect("tempdir");
    let path = db_path(&dir);
    let store = Store::open(&path).expect("open");
    let run = store.create_run(&agent(), "x").expect("create_run");
    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|t| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let own = Store::open(&path).expect("open per task");
                barrier.wait();
                for i in 0..100 {
                    own.append(run, msg(t * 100 + i)).expect("append");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("task panicked");
    }

    assert_800_gapless(&store, run);
}

// 3. Given events 1..=10, when events_since(run, 4) runs, then it returns 5..=10 in order.
#[test]
fn s3_events_since_returns_tail_in_order() {
    let store = Store::open_in_memory().expect("open");
    let run = store.create_run(&agent(), "x").expect("create_run");
    for i in 1..=10 {
        store.append(run, msg(i)).expect("append");
    }

    let tail = store.events_since(run, 4).expect("events_since");
    let got: Vec<u64> = tail.iter().map(|e| e.seq).collect();
    assert_eq!(got, (5..=10).collect::<Vec<u64>>());
    for ev in &tail {
        assert_eq!(ev.run_id, run);
        assert_eq!(ev.kind, msg(ev.seq as usize));
    }
}

// 4. Given a file-backed store, when it is dropped and reopened, then runs, events and model calls
//    are all still there.
#[test]
fn s4_reopen_keeps_runs_events_and_model_calls() {
    let dir = TempDir::new().expect("tempdir");
    let path = db_path(&dir);

    let (run, run_row, events) = {
        let store = Store::open(&path).expect("open");
        let run = store
            .create_run(&agent(), "persist me")
            .expect("create_run");
        store.append(run, msg(1)).expect("append");
        store.append(run, msg(2)).expect("append");
        store
            .record_model_call(&call(run, Some(1_500_000)))
            .expect("record_model_call");
        let row = store.get_run(run).expect("get_run").expect("run exists");
        let events = store.events_since(run, 0).expect("events_since");
        (run, row, events)
    };

    let store = Store::open(&path).expect("reopen");
    let row = store
        .get_run(run)
        .expect("get_run")
        .expect("run still there");
    assert_eq!(row, run_row);
    assert_eq!(row.agent, "store-tester");
    assert_eq!(row.input, "persist me");
    assert_eq!(row.status, RunStatus::Queued);
    assert_eq!(store.events_since(run, 0).expect("events_since"), events);
    assert_eq!(events.len(), 2);
    assert_eq!(store.spent(run).expect("spent"), MicroUsd(1_500_000));
}

// 5. Given two Store handles on the same file (CLI and engine), when one appends, then the other
//    sees it on its next events_since.
#[test]
fn s5_second_handle_sees_appends() {
    let dir = TempDir::new().expect("tempdir");
    let path = db_path(&dir);
    let cli = Store::open(&path).expect("open cli");
    let engine = Store::open(&path).expect("open engine");

    let run = cli.create_run(&agent(), "x").expect("create_run");
    assert!(
        engine
            .events_since(run, 0)
            .expect("events_since")
            .is_empty()
    );

    let ev = cli.append(run, msg(1)).expect("append");
    let seen = engine.events_since(run, 0).expect("events_since");
    assert_eq!(seen, vec![ev]);

    let ev2 = engine.append(run, msg(2)).expect("append");
    assert_eq!(ev2.seq, 2);
    let seen = cli.events_since(run, 1).expect("events_since");
    assert_eq!(seen, vec![ev2]);
}

// 6. Given a run in Running, when transition(expected: Queued, next: Provisioning) is called, then it
//    fails with a conflict error and nothing is written.
#[test]
fn s6_stale_expected_status_is_conflict_and_writes_nothing() {
    let store = Store::open_in_memory().expect("open");
    let run = store.create_run(&agent(), "x").expect("create_run");
    drive_to(&store, run, &RunStatus::Running);
    let row_before = store.get_run(run).expect("get_run");
    let events_before = store.events_since(run, 0).expect("events_since");

    let res = store.transition(run, &RunStatus::Queued, RunStatus::Provisioning);
    match res {
        Err(StoreError::Conflict {
            run: r,
            expected,
            actual,
        }) => {
            assert_eq!(r, run);
            assert_eq!(expected, RunStatus::Queued);
            assert_eq!(actual, RunStatus::Running);
        }
        other => panic!("expected StoreError::Conflict, got {other:?}"),
    }

    assert_eq!(store.get_run(run).expect("get_run"), row_before);
    assert_eq!(status_of(&store, run), RunStatus::Running);
    assert_eq!(
        store.events_since(run, 0).expect("events_since"),
        events_before
    );
}

// 7. Given an illegal transition (Completed -> Running), then it fails and nothing is written.
#[test]
fn s7_illegal_transition_fails_and_writes_nothing() {
    let store = Store::open_in_memory().expect("open");
    let run = store.create_run(&agent(), "x").expect("create_run");
    drive_to(&store, run, &RunStatus::Completed);
    assert_eq!(status_of(&store, run), RunStatus::Completed);
    let row_before = store.get_run(run).expect("get_run");
    let events_before = store.events_since(run, 0).expect("events_since");

    let res = store.transition(run, &RunStatus::Completed, RunStatus::Running);
    match res {
        Err(StoreError::IllegalTransition { from, to }) => {
            assert_eq!(from, RunStatus::Completed);
            assert_eq!(to, RunStatus::Running);
        }
        other => panic!("expected StoreError::IllegalTransition, got {other:?}"),
    }

    assert_eq!(store.get_run(run).expect("get_run"), row_before);
    assert_eq!(
        store.events_since(run, 0).expect("events_since"),
        events_before
    );
}

// 8. Given a successful transition, then a StatusChanged event exists with the matching from/to,
//    written atomically with the status.
#[test]
fn s8_transition_writes_status_changed_event_with_status() {
    let dir = TempDir::new().expect("tempdir");
    let path = db_path(&dir);
    let store = Store::open(&path).expect("open");
    let observer = Store::open(&path).expect("open observer");
    let run = store.create_run(&agent(), "x").expect("create_run");
    let before = store.events_since(run, 0).expect("events_since");

    store
        .transition(run, &RunStatus::Queued, RunStatus::Provisioning)
        .expect("Queued -> Provisioning");

    // Both the status and exactly one new StatusChanged event are visible, also from another
    // handle (committed together).
    for s in [&store, &observer] {
        assert_eq!(status_of(s, run), RunStatus::Provisioning);
        let after = s.events_since(run, 0).expect("events_since");
        assert_eq!(after.len(), before.len() + 1);
        let last = after.last().expect("new event");
        assert_eq!(
            last.kind,
            EventKind::StatusChanged {
                from: RunStatus::Queued,
                to: RunStatus::Provisioning,
            }
        );
    }

    // A second transition records its own from/to.
    let reason = kestrel_types::SuspendReason::Approval(ApprovalId::new());
    store
        .transition(run, &RunStatus::Provisioning, RunStatus::Running)
        .expect("Provisioning -> Running");
    store
        .transition(
            run,
            &RunStatus::Running,
            RunStatus::Suspended(reason.clone()),
        )
        .expect("Running -> Suspended");
    let changes: Vec<(RunStatus, RunStatus)> = store
        .events_since(run, 0)
        .expect("events_since")
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::StatusChanged { from, to } => Some((from, to)),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes,
        vec![
            (RunStatus::Queued, RunStatus::Provisioning),
            (RunStatus::Provisioning, RunStatus::Running),
            (RunStatus::Running, RunStatus::Suspended(reason.clone())),
        ]
    );
    assert_eq!(status_of(&store, run), RunStatus::Suspended(reason));
}

// 9. Given model calls costing 1.25 and 0.75 USD plus one with cost: None, when spent runs, then it
//    returns exactly 2.000000 USD.
#[test]
fn s9_spent_sums_known_costs_exactly() {
    let store = Store::open_in_memory().expect("open");
    let run = store.create_run(&agent(), "x").expect("create_run");
    let other = store.create_run(&agent(), "y").expect("create_run");

    store
        .record_model_call(&call(run, Some(1_250_000)))
        .expect("record");
    store
        .record_model_call(&call(run, Some(750_000)))
        .expect("record");
    store.record_model_call(&call(run, None)).expect("record");
    store
        .record_model_call(&call(other, Some(9_000_000)))
        .expect("record other run");

    let spent = store.spent(run).expect("spent");
    assert_eq!(spent, MicroUsd(2_000_000));
    assert_eq!(spent.to_string(), "$2.000000");
}

// 10. Given two control commands pushed, when take_control runs twice, then the first call returns
//     both and the second returns none.
#[test]
fn s10_take_control_claims_each_command_once() {
    let store = Store::open_in_memory().expect("open");
    let run = store.create_run(&agent(), "x").expect("create_run");
    let approval = ApprovalId::new();

    let a = store
        .push_control(run, ControlCommand::Pause)
        .expect("push");
    let b = store
        .push_control(run, ControlCommand::Approve(approval))
        .expect("push");
    assert_ne!(a, b);

    let first = store.take_control().expect("take");
    assert_eq!(
        first,
        vec![
            (a, run, ControlCommand::Pause),
            (b, run, ControlCommand::Approve(approval)),
        ]
    );
    let second = store.take_control().expect("take");
    assert!(second.is_empty(), "second take returned {second:?}");
}

// 10. "... even with two consumers racing": two handles on the same file call take_control while a
//     producer pushes. Every command is claimed by exactly one consumer.
#[test]
fn s10_take_control_racing_consumers_claim_once() {
    const N: usize = 200;
    let dir = TempDir::new().expect("tempdir");
    let path = db_path(&dir);
    let producer = Store::open(&path).expect("open producer");
    let run = producer.create_run(&agent(), "x").expect("create_run");
    let done = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(3));

    let consumers: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let done = Arc::clone(&done);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = Store::open(&path).expect("open consumer");
                barrier.wait();
                let mut got = Vec::new();
                loop {
                    let finished = done.load(Ordering::SeqCst);
                    let batch = store.take_control().expect("take");
                    let empty = batch.is_empty();
                    got.extend(batch);
                    if finished && empty {
                        return got;
                    }
                }
            })
        })
        .collect();

    barrier.wait();
    let mut pushed = BTreeMap::new();
    for i in 0..N {
        let cmd = ControlCommand::SetBudget(MicroUsd(i as i64));
        let id = producer.push_control(run, cmd.clone()).expect("push");
        assert!(pushed.insert(id, (run, cmd)).is_none(), "duplicate id {id}");
    }
    done.store(true, Ordering::SeqCst);

    let mut claimed = BTreeMap::new();
    for c in consumers {
        for (id, r, cmd) in c.join().expect("consumer panicked") {
            assert!(
                claimed.insert(id, (r, cmd)).is_none(),
                "command {id} claimed twice"
            );
        }
    }
    assert_eq!(claimed, pushed, "every command claimed exactly once");
    assert!(producer.take_control().expect("take").is_empty());
}

// 11. Given a database with a newer schema version than this binary knows, when opened, then it fails
//     with a clear "upgrade kestrel" error instead of migrating down.
#[test]
fn s11_newer_schema_refuses_with_upgrade_error() {
    let dir = TempDir::new().expect("tempdir");
    let path = db_path(&dir);
    {
        let _store = Store::open(&path).expect("open");
    }

    let newer = SCHEMA_VERSION + 1;
    {
        let conn = rusqlite::Connection::open(&path).expect("raw open");
        conn.pragma_update(None, "user_version", newer)
            .expect("bump user_version");
    }
    let before = schema_snapshot(&path);
    assert_eq!(before.0, newer);

    match Store::open(&path) {
        Ok(_) => panic!("opening a newer schema must fail"),
        Err(e) => {
            assert!(
                e.to_string().contains("upgrade kestrel"),
                "error should tell the user to upgrade kestrel, got: {e}"
            );
            match e {
                StoreError::SchemaTooNew { found, supported } => {
                    assert_eq!(found, newer);
                    assert_eq!(supported, SCHEMA_VERSION);
                }
                other => panic!("expected StoreError::SchemaTooNew, got {other:?}"),
            }
        }
    }

    // Not migrated down: version and schema untouched.
    assert_eq!(schema_snapshot(&path), before);
}

// 12. Given migrations already applied, when opened again, then nothing changes (idempotent).
#[test]
fn s12_reopen_is_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    let path = db_path(&dir);

    let (run, events) = {
        let store = Store::open(&path).expect("open");
        let run = store.create_run(&agent(), "x").expect("create_run");
        store.append(run, msg(1)).expect("append");
        (run, store.events_since(run, 0).expect("events_since"))
    };
    let first = schema_snapshot(&path);
    assert_eq!(first.0, SCHEMA_VERSION);

    for _ in 0..2 {
        {
            let store = Store::open(&path).expect("reopen");
            assert_eq!(store.events_since(run, 0).expect("events_since"), events);
            assert_eq!(status_of(&store, run), RunStatus::Queued);
        }
        assert_eq!(schema_snapshot(&path), first, "reopen changed the schema");
    }
}
