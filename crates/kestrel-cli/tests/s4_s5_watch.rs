//! Scenarios 4 and 5: `kestrel watch` on unknown, finished and live runs, and `run --watch`.
//!
//! With `--json`, watch prints one `kestrel_types::Event` per line (JSON Lines).

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{ChildGuard, Fixture, agent, path_str, running_run, walk};
use kestrel_types::{Event, EventKind, RunId, RunStatus};
use predicates::prelude::*;

fn event(line: &str) -> Event {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not an Event ({e}): {line}"))
}

fn message(text: &str) -> EventKind {
    EventKind::AgentMessage { text: text.into() }
}

#[test]
fn s4_watch_unknown_run_exits_1() {
    let f = Fixture::new();
    f.kestrel()
        .args(["watch", &RunId::new().to_string()])
        .timeout(common::DEADLINE)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no such run"));
}

#[test]
fn s4_watch_unknown_run_exits_1_with_json_too() {
    let f = Fixture::new();
    f.kestrel()
        .args(["watch", &RunId::new().to_string(), "--json"])
        .timeout(common::DEADLINE)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no such run"));
}

#[test]
fn s4_watch_malformed_id_exits_2() {
    let f = Fixture::new();
    f.kestrel()
        .args(["watch", "not-a-run-id"])
        .timeout(common::DEADLINE)
        .assert()
        .code(2);
}

/// A finished run with a few events: status changes, a message, a finish event.
fn finished_run(f: &Fixture) -> RunId {
    let store = f.store();
    let id = running_run(&store);
    store
        .append(id, message("hello from the agent"))
        .expect("append");
    store
        .append(
            id,
            EventKind::RunFinished {
                summary: Some("done".into()),
            },
        )
        .expect("append");
    walk(&store, id, RunStatus::Running, &[RunStatus::Completed]);
    id
}

#[test]
fn s5_watch_finished_run_prints_all_events_and_exits_0() {
    let f = Fixture::new();
    let id = finished_run(&f);
    let expected = f.store().events_since(id, 0).expect("events");

    let out = f
        .kestrel()
        .args(["watch", &id.to_string(), "--json"])
        .timeout(common::DEADLINE)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let got: Vec<Event> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(event)
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn s5_watch_finished_run_text_shows_the_events() {
    let f = Fixture::new();
    let id = finished_run(&f);
    f.kestrel()
        .args(["watch", &id.to_string()])
        .timeout(common::DEADLINE)
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from the agent"))
        .stdout(predicate::str::contains("Completed"));
}

#[test]
fn s5_watch_follows_new_events_live_then_exits_0() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_run(&store);
    let before = store.events_since(id, 0).expect("events");
    let last_seq = before.last().expect("status events").seq;

    let mut cmd = f.kestrel_std();
    cmd.args(["watch", &id.to_string(), "--json"]);
    let mut watch = ChildGuard::spawn(cmd);
    let mut lines = watch.stdout_lines();

    // The backlog first.
    lines.until("backlog", |l| event(l).seq == last_seq);

    // New events from a second handle show up without restarting watch.
    for text in ["live-1", "live-2"] {
        store.append(id, message(text)).expect("append");
        let line = lines.until(text, |l| event(l).kind == message(text));
        assert_eq!(event(&line).run_id, id);
    }
    assert!(
        watch.child.try_wait().expect("try_wait").is_none(),
        "watch exited while the run was still running"
    );

    // Finishing the run ends watch with exit 0, after it prints the final status change.
    walk(&store, id, RunStatus::Running, &[RunStatus::Completed]);
    let rest = lines.rest();
    assert!(
        rest.iter().any(|l| matches!(
            event(l).kind,
            EventKind::StatusChanged {
                to: RunStatus::Completed,
                ..
            }
        )),
        "final status change missing: {rest:#?}"
    );
    let status = watch.wait_exit("watch to exit");
    assert!(
        status.success(),
        "watch exit {status:?}, stderr: {}",
        watch.stderr()
    );

    // Every event printed exactly once, in seq order.
    let seqs: Vec<u64> = lines.seen.iter().map(|l| event(l).seq).collect();
    let all: Vec<u64> = store
        .events_since(id, 0)
        .expect("events")
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, all);
}

#[test]
fn s5_run_with_watch_prints_id_then_follows_until_finished() {
    let f = Fixture::new();
    let good = f.good_agent();
    let mut cmd = f.kestrel_std();
    cmd.args(["run", path_str(&good), "--input", "x", "--watch"]);
    let mut run = ChildGuard::spawn(cmd);
    let mut lines = run.stdout_lines();

    let id: RunId = lines
        .next_line("run id")
        .trim()
        .parse()
        .expect("first line is the run id");

    // Play the engine from the test.
    let store = f.store();
    common::poll("run to be queued", || store.get_run(id).ok().flatten());
    walk(
        &store,
        id,
        RunStatus::Queued,
        &[RunStatus::Provisioning, RunStatus::Running],
    );
    store.append(id, message("working on it")).expect("append");
    lines.until("agent message", |l| l.contains("working on it"));
    walk(&store, id, RunStatus::Running, &[RunStatus::Completed]);

    let status = run.wait_exit("run --watch to exit");
    assert!(
        status.success(),
        "exit {status:?}, stderr: {}",
        run.stderr()
    );
}

#[test]
fn s5_watch_queued_run_waits_for_it_to_start() {
    let f = Fixture::new();
    let store = f.store();
    let id = store.create_run(&agent(), "x").expect("create");

    let mut cmd = f.kestrel_std();
    cmd.args(["watch", &id.to_string(), "--json"]);
    let mut watch = ChildGuard::spawn(cmd);
    let mut lines = watch.stdout_lines();

    walk(&store, id, RunStatus::Queued, &[RunStatus::Provisioning]);
    lines.until("provisioning", |l| {
        matches!(
            event(l).kind,
            EventKind::StatusChanged {
                to: RunStatus::Provisioning,
                ..
            }
        )
    });
    walk(&store, id, RunStatus::Provisioning, &[RunStatus::Cancelled]);
    let status = watch.wait_exit("watch to exit");
    assert!(
        status.success(),
        "exit {status:?}, stderr: {}",
        watch.stderr()
    );
}
