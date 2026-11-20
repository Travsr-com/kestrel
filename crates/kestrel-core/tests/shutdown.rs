//! Issue #19 (engine side): graceful shutdown, no leaked sandboxes, the label sweep at recovery,
//! and keeping the scheduler alive after a store error. The serve side (signals, the grace
//! period, a second Ctrl-C) is #25.
//!
//! Contract the tests pin down:
//!
//! * `Engine::shutdown(grace)` cancels the run actors, stops and destroys every live sandbox
//!   (running, paused, waiting for approval, still being created, or in the final destroy of a
//!   finished run), waits at most `grace` in total, and reports what it did not finish. When it
//!   returns, `run_forever` has returned. Calling it again, or with nothing running, is fine.
//! * It does not change any run's status. A `Running` run stays `Running` and a `Provisioning`
//!   run stays `Provisioning` in the store, so `recover` (unchanged) turns them into
//!   `Suspended(Error("engine restarted"))` and `Failed("engine restarted while provisioning")`
//!   on the next start. `Queued` runs stay queued and are never started by a shutdown.
//! * `recover` also asks every driver for `leftovers` and destroys what it lists, so a container
//!   a crash left behind (no `SandboxProvisioned` event) is removed too.
//!
//! Everything runs on a paused clock with fakes. `tests/shutdown_support` holds the `Probe`
//! driver (wraps `FakeDriver`, tracks completed destroys, can hold or fail them).

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod shutdown_support;

use std::time::Duration;

use common::*;
use kestrel_core::{EngineError, ShutdownReport};
use kestrel_sandbox::RUN_LABEL;
use kestrel_store::ControlCommand;
use kestrel_types::{ApprovalId, EventKind, MicroUsd, RunId, RunStatus, SuspendReason};
use shutdown_support::{harness, harness_on};
use tokio_util::sync::CancellationToken;

use RunStatus::{Cancelled, Completed, Provisioning, Queued, Running, Suspended};

const GRACE: Duration = Duration::from_secs(30);

fn sorted(mut ids: Vec<RunId>) -> Vec<RunId> {
    ids.sort();
    ids
}

fn logged(h: &Harness, id: RunId, needle: &str) -> bool {
    h.has_event(
        id,
        |k| matches!(k, EventKind::AgentLog { line } if line.contains(needle)),
    )
}

/// `run_forever` has returned, and with `Ok`. Call it right after `shutdown` returned.
async fn assert_run_forever_returned(h: &mut Harness) {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    let task = h.task.take().expect("run_forever was started");
    assert!(
        task.is_finished(),
        "run_forever must have returned when shutdown returns"
    );
    task.await.expect("join").expect("run_forever returned Ok");
}

async fn running(h: &Harness, id: RunId) {
    until("running", || h.is(id, &Running)).await;
}

fn is_empty(report: &ShutdownReport) -> bool {
    report.destroyed.is_empty() && report.failed.is_empty() && report.timed_out.is_empty()
}

// 1 ---------------------------------------------------------------------------------------------
// Shutdown destroys every live sandbox.

#[tokio::test(start_paused = true)]
async fn s1_shutdown_destroys_running_paused_and_approval_waiting_sandboxes() {
    let (mut h, p) = harness(4);
    let _sa = h.driver.live();
    let _sb = h.driver.live();
    let sc = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let b = h.submit(spec()).await;
    running(&h, b).await;
    h.control(b, ControlCommand::Pause);
    let manual = Suspended(SuspendReason::Manual);
    until("paused", || h.is(b, &manual)).await;
    let c = h.submit(spec()).await;
    running(&h, c).await;
    let aid = ApprovalId::new();
    sc.line(&format!("approval:{aid}:gh.comment"));
    let waiting = Suspended(SuspendReason::Approval(aid));
    until("waiting for approval", || h.is(c, &waiting)).await;

    let report = h.engine.shutdown(GRACE).await;

    assert_eq!(sorted(report.destroyed.clone()), sorted(vec![a, b, c]));
    assert!(
        report.failed.is_empty() && report.timed_out.is_empty(),
        "{report:?}"
    );
    assert!(p.unreleased().is_empty(), "leaked {:?}", p.unreleased());
    assert_run_forever_returned(&mut h).await;
    // No status was touched.
    assert!(h.is(a, &Running));
    assert!(h.is(b, &manual));
    assert!(h.is(c, &waiting));
    // The next start finds them as it always did.
    let rec = h.engine.recover().await.expect("recover after shutdown");
    assert_eq!(rec.interrupted, vec![a]);
    assert_eq!(sorted(rec.suspended), sorted(vec![b, c]));
    assert!(h.is(
        a,
        &Suspended(SuspendReason::Error("engine restarted".into()))
    ));
}

#[tokio::test(start_paused = true)]
async fn s1_shutdown_leaves_queued_runs_queued_and_starts_nothing_new() {
    let (mut h, p) = harness(1);
    let _sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let queued = h.submit(spec()).await;
    settle().await;
    assert!(h.is(queued, &Queued));

    let report = h.engine.shutdown(GRACE).await;

    assert_eq!(report.destroyed, vec![a]);
    assert!(p.unreleased().is_empty());
    settle().await;
    assert!(h.is(queued, &Queued), "a shut-down engine starts no run");
    assert_eq!(h.driver.count(&Call::Create), 1);
    assert_run_forever_returned(&mut h).await;
}

#[tokio::test(start_paused = true)]
async fn s1_every_sandbox_is_created_with_the_run_label() {
    let (mut h, _p) = harness(4);
    let _sa = h.driver.live();
    let _sb = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;
    until("both running", || h.is(a, &Running) && h.is(b, &Running)).await;

    let mut labels: Vec<String> = h
        .driver
        .specs()
        .iter()
        .map(|s| s.labels.get(RUN_LABEL).cloned().unwrap_or_default())
        .collect();
    labels.sort();
    let mut want = vec![a.to_string(), b.to_string()];
    want.sort();
    assert_eq!(labels, want, "label `{RUN_LABEL}` is the run id");
}

// 2 ---------------------------------------------------------------------------------------------
// The three leak windows from the issue.

#[tokio::test(start_paused = true)]
async fn s1_a_create_still_in_flight_is_destroyed_once_it_returns() {
    let (mut h, p) = harness(4);
    let gate = h.driver.gate_create();
    h.start();
    let id = h.submit(spec()).await;
    until("create in flight", || {
        h.is(id, &Provisioning) && h.driver.count(&Call::Create) == 1
    })
    .await;

    // `create` returns while shutdown is waiting for it.
    let (report, ()) = tokio::join!(h.engine.shutdown(GRACE), async {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        gate.notify_one();
    });

    assert_eq!(report.destroyed, vec![id], "{report:?}");
    assert!(
        report.failed.is_empty() && report.timed_out.is_empty(),
        "{report:?}"
    );
    assert_eq!(p.destroyed(), vec!["fake-1".to_string()]);
    assert!(p.unreleased().is_empty());
    assert_eq!(
        h.driver.starts(),
        0,
        "the agent is not started during shutdown"
    );
    assert_run_forever_returned(&mut h).await;
    // The run is still `Provisioning`; the next start fails it.
    assert!(h.is(id, &Provisioning));
    h.engine.recover().await.expect("recover");
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains("engine restarted"), "{msg}");
}

#[tokio::test(start_paused = true)]
async fn s1_a_finished_run_in_its_final_destroy_is_destroyed_by_shutdown() {
    let (mut h, p) = harness(4);
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    running(&h, id).await;
    p.hold_destroys();
    s.line("ok:done");
    s.exit(0);
    until("completed", || h.is(id, &Completed)).await;
    until("final destroy started", || p.attempts().len() == 1).await;
    assert!(p.destroyed().is_empty(), "the destroy is still in flight");

    let (report, ()) = tokio::join!(h.engine.shutdown(GRACE), async {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        p.release_destroys();
    });

    assert_eq!(report.destroyed, vec![id], "{report:?}");
    assert!(
        report.failed.is_empty() && report.timed_out.is_empty(),
        "{report:?}"
    );
    assert!(p.unreleased().is_empty(), "leaked {:?}", p.unreleased());
    assert!(h.is(id, &Completed));
    assert_run_forever_returned(&mut h).await;
}

#[tokio::test(start_paused = true)]
async fn s1_a_run_cancelled_while_provisioning_does_not_leak_when_shutdown_follows() {
    let (mut h, p) = harness(4);
    let gate = h.driver.gate_create();
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("provisioning", || {
        h.is(id, &Provisioning) && h.driver.count(&Call::Create) == 1
    })
    .await;
    h.control(id, ControlCommand::Cancel);
    until("cancelled", || h.is(id, &Cancelled)).await;
    assert!(
        p.attempts().is_empty(),
        "nothing to destroy before create returns"
    );

    let (report, ()) = tokio::join!(h.engine.shutdown(GRACE), async {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        gate.notify_one();
    });

    assert_eq!(report.destroyed, vec![id], "{report:?}");
    assert!(p.unreleased().is_empty(), "leaked {:?}", p.unreleased());
    assert_eq!(h.driver.starts(), 0);
    assert!(h.is(id, &Cancelled));
    assert_run_forever_returned(&mut h).await;
}

// 3 ---------------------------------------------------------------------------------------------
// Grace period, failures, repeat calls.

#[tokio::test(start_paused = true)]
async fn s1_shutdown_reports_timed_out_when_a_destroy_outlasts_the_grace_period() {
    let (mut h, p) = harness(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    running(&h, id).await;
    p.hold_destroys();
    let grace = Duration::from_secs(5);

    let start = tokio::time::Instant::now();
    let report = tokio::time::timeout(Duration::from_secs(60), h.engine.shutdown(grace))
        .await
        .expect("shutdown must not hang past its grace period");

    assert_eq!(report.timed_out, vec![id], "{report:?}");
    assert!(
        report.destroyed.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    let took = start.elapsed();
    assert!(took >= grace, "waited the whole grace period, not {took:?}");
    assert!(took <= grace + Duration::from_secs(1), "waited {took:?}");
    assert_run_forever_returned(&mut h).await;
    // Once the driver recovers, the sweep at the next start finds the sandbox through its label.
    p.release_destroys();
    p.set_leftovers(&["fake-1"]);
    h.engine.recover().await.expect("recover");
    until("swept", || p.unreleased().is_empty()).await;
}

#[tokio::test(start_paused = true)]
async fn s1_shutdown_reports_the_runs_whose_destroy_failed() {
    let (mut h, p) = harness(4);
    let _sa = h.driver.live();
    let _sb = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let b = h.submit(spec()).await;
    running(&h, b).await;
    p.fail_destroys_of(&["fake-1"]);

    let report = h.engine.shutdown(GRACE).await;

    assert_eq!(report.destroyed, vec![b], "{report:?}");
    assert!(report.timed_out.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].0, a);
    assert!(
        report.failed[0].1.contains("cannot remove fake-1"),
        "the driver's error text is kept: {:?}",
        report.failed[0].1
    );
    assert_eq!(p.unreleased(), vec!["fake-1".to_string()]);
}

#[tokio::test(start_paused = true)]
async fn s1_shutdown_twice_is_safe_and_the_second_has_nothing_left_to_do() {
    let (mut h, p) = harness(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    running(&h, id).await;

    let first = h.engine.shutdown(GRACE).await;
    let second = h.engine.shutdown(GRACE).await;

    assert_eq!(first.destroyed, vec![id]);
    assert!(is_empty(&second), "{second:?}");
    assert_eq!(p.attempts(), vec!["fake-1".to_string()], "destroyed once");
    assert_run_forever_returned(&mut h).await;
}

#[tokio::test(start_paused = true)]
async fn s1_shutdown_with_nothing_running_is_a_no_op() {
    let (h, p) = harness(4);
    // Never started.
    assert!(is_empty(&h.engine.shutdown(GRACE).await));
    assert!(is_empty(&h.engine.shutdown(Duration::ZERO).await));
    assert!(p.attempts().is_empty());

    // Started, but no run was ever submitted.
    let (mut h, p) = harness(4);
    h.start();
    settle().await;
    assert!(is_empty(&h.engine.shutdown(GRACE).await));
    assert_run_forever_returned(&mut h).await;
    assert!(p.attempts().is_empty());
}

// 4 ---------------------------------------------------------------------------------------------
// Slots and the waiting line are reset when run_forever starts and ends.

#[tokio::test(start_paused = true)]
async fn s1_after_shutdown_a_second_run_forever_has_a_free_slot_again() {
    let (mut h, _p) = harness(1);
    let _sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let b = h.submit(spec()).await;
    settle().await;
    assert!(h.is(b, &Queued), "the cap of 1 is full");

    h.engine.shutdown(GRACE).await;
    assert_run_forever_returned(&mut h).await;
    h.engine.recover().await.expect("recover");
    let _sb = h.driver.live();
    h.start();

    until("queued run starts on the free slot", || h.is(b, &Running)).await;
}

#[tokio::test(start_paused = true)]
async fn s1_run_forever_gives_back_the_slots_of_the_actors_it_aborts_on_exit() {
    let (mut h, _p) = harness(1);
    let _sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let b = h.submit(spec()).await;
    settle().await;
    assert!(h.is(b, &Queued));

    // Stop through the token (no `shutdown`): the actors are aborted, `a` is still `Running` in
    // the store, and nothing released its slot.
    h.shutdown.cancel();
    let task = h.task.take().expect("started");
    task.await.expect("join").expect("run_forever ok");
    h.shutdown = CancellationToken::new();
    let _sb = h.driver.live();
    h.start();

    until("queued run starts on the free slot", || h.is(b, &Running)).await;
}

// 5 ---------------------------------------------------------------------------------------------
// recover and run_forever do not overlap.

#[tokio::test(start_paused = true)]
async fn s1_recover_while_run_forever_is_active_is_refused_and_changes_nothing() {
    let (mut h, p) = harness(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    running(&h, id).await;

    let err = h.engine.recover().await.expect_err("refused while running");

    assert!(matches!(err, EngineError::AlreadyRunning), "{err:?}");
    assert!(!err.to_string().is_empty());
    assert!(h.is(id, &Running), "the live run is untouched");
    assert!(p.attempts().is_empty(), "its sandbox is untouched");
    assert!(h.driver.leaked().contains(&"fake-1".to_string()));
}

#[tokio::test(start_paused = true)]
async fn s1_recover_works_before_run_forever_and_again_after_it_stopped() {
    let (mut h, _p) = harness(4);
    h.engine.recover().await.expect("before run_forever");

    h.start();
    settle().await;
    h.shutdown.cancel();
    let task = h.task.take().expect("started");
    task.await.expect("join").expect("run_forever ok");
    h.engine
        .recover()
        .await
        .expect("after run_forever returned");

    // And after a second start and a graceful shutdown.
    h.shutdown = CancellationToken::new();
    h.start();
    settle().await;
    h.engine.shutdown(GRACE).await;
    h.engine.recover().await.expect("after shutdown");
}

// 6 ---------------------------------------------------------------------------------------------
// SetBudget for a Queued run.

#[tokio::test(start_paused = true)]
async fn s1_set_budget_for_a_queued_run_updates_the_stored_budget_and_the_start_uses_it() {
    let (mut h, _p) = harness(1);
    let sx = h.driver.live();
    let sa = h.driver.live();
    h.start();
    let blocker = h.submit(spec()).await;
    running(&h, blocker).await;
    let a = h.submit(spec()).await;
    settle().await;
    assert!(h.is(a, &Queued));

    h.control(a, ControlCommand::SetBudget(MicroUsd(100_000)));
    settle().await;

    let stored = h.store.run_spec(a).unwrap().expect("spec").budget.limit;
    assert_eq!(stored, MicroUsd(100_000), "the queued run's budget changed");
    assert!(h.is(a, &Queued));
    assert!(!logged(&h, a, "ignored"), "not dropped as ignored");

    // Free the slot; the run starts with the new limit and is suspended at it.
    sx.line("ok:done");
    sx.exit(0);
    running(&h, a).await;
    h.store.record_model_call(&model_call(a, 100_000)).unwrap();
    sa.line("msg:spent");
    until("suspended at the new limit", || {
        h.is(a, &Suspended(SuspendReason::Budget))
    })
    .await;
}

// 7 ---------------------------------------------------------------------------------------------
// A store error on one command.

/// Removes the stored spec of `run`, so `Store::set_budget` fails for it with `NotFound`.
fn break_spec(db: &std::path::Path, run: RunId) {
    let conn = rusqlite::Connection::open(db).expect("second connection");
    let n = conn
        .execute("DELETE FROM run_specs WHERE run_id = ?1", [run.to_string()])
        .expect("delete spec");
    assert_eq!(n, 1);
}

#[tokio::test(start_paused = true)]
async fn s1_a_store_error_on_one_command_is_logged_on_that_run_and_scheduling_goes_on() {
    let (mut h, _p) = harness(4);
    let bad = h.submit(spec()).await;
    let victim = h.submit(spec()).await;
    break_spec(&h.db, bad);
    // One claimed batch: the first command hits a store error, the second must still apply.
    h.control(bad, ControlCommand::SetBudget(MicroUsd(5)));
    h.control(victim, ControlCommand::Cancel);
    h.start();

    until("the rest of the batch was applied", || {
        h.is(victim, &Cancelled)
    })
    .await;
    // The store error is logged on that run (SetBudget is not "ignored" for a Queued run).
    until("the error is logged on the run", || {
        h.has_event(
            bad,
            |k| matches!(k, EventKind::AgentLog { line } if !line.contains("ignored")),
        )
    })
    .await;
    assert!(
        !h.task.as_ref().expect("started").is_finished(),
        "run_forever keeps going"
    );

    // Still scheduling: a new run starts and a later command is applied.
    let _s = h.driver.live();
    let later = h.submit(spec()).await;
    running(&h, later).await;
    h.control(later, ControlCommand::Cancel);
    until("later command applied", || h.is(later, &Cancelled)).await;
    assert!(!h.task.as_ref().expect("started").is_finished());
}

#[tokio::test(start_paused = true)]
async fn s1_a_store_error_that_breaks_the_tick_ends_run_forever_with_an_error_and_shutdown_still_cleans_up()
 {
    let (mut h, p) = harness(4);
    let _s = h.driver.live();
    h.start();
    h.submit(spec()).await;
    until("sandbox started", || h.driver.starts() == 1).await;
    settle().await;
    // The scheduler can no longer list runs.
    let conn = rusqlite::Connection::open(&h.db).expect("second connection");
    conn.execute("DROP TABLE runs", []).expect("drop runs");

    until("run_forever ended", || {
        h.task.as_ref().expect("started").is_finished()
    })
    .await;
    let err = h
        .task
        .take()
        .expect("started")
        .await
        .expect("join")
        .expect_err("a broken store is an error, not a silent stop");
    assert!(matches!(err, EngineError::Store(_)), "{err:?}");
    assert!(!err.to_string().is_empty());

    let report = tokio::time::timeout(Duration::from_secs(60), h.engine.shutdown(GRACE))
        .await
        .expect("shutdown after a failed run_forever must not hang");

    assert!(
        report.failed.is_empty() && report.timed_out.is_empty(),
        "{report:?}"
    );
    assert!(p.unreleased().is_empty(), "leaked {:?}", p.unreleased());
}

// 8 ---------------------------------------------------------------------------------------------
// Leftover sweep at recovery.

#[tokio::test(start_paused = true)]
async fn s1_recover_destroys_a_container_a_crash_left_behind_without_an_event() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("kestrel.db");
    // The previous engine died while `create` was in flight: the run is `Provisioning`, no
    // `SandboxProvisioned` event exists, but the container carries the run label.
    let id = {
        let first = Harness::on(&db, 4);
        let id = first.submit(spec()).await;
        first.store.transition(id, &Queued, Provisioning).unwrap();
        id
    };
    let (h, p) = harness_on(&db, 4);
    p.set_leftovers(&["lost-1", "lost-2"]);

    let report = h.engine.recover().await.expect("recover");

    let mut swept = p.destroyed();
    swept.sort();
    assert_eq!(swept, vec!["lost-1".to_string(), "lost-2".to_string()]);
    assert!(report.interrupted.is_empty() && report.suspended.is_empty());
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains("engine restarted"), "{msg}");
}

#[tokio::test(start_paused = true)]
async fn s1_the_sweep_destroys_only_what_the_driver_lists() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("kestrel.db");
    let first = Harness::on(&db, 4);
    let a = first.submit(spec()).await;
    first.store.transition(a, &Queued, Provisioning).unwrap();
    first
        .store
        .append(
            a,
            EventKind::SandboxProvisioned {
                driver: "fake".into(),
                sandbox_id: "old-1".into(),
            },
        )
        .unwrap();
    first.store.transition(a, &Provisioning, Running).unwrap();
    drop(first);
    let (h, p) = harness_on(&db, 4);
    // `old-1` is the run's recorded sandbox and is also listed; `unrelated` is some other
    // container on the host without the label, which the driver does not list.
    p.set_leftovers(&["old-1", "orphan-1"]);

    let report = h.engine.recover().await.expect("recover");

    assert_eq!(report.interrupted, vec![a]);
    let destroyed = p.destroyed();
    assert!(destroyed.contains(&"old-1".to_string()), "{destroyed:?}");
    assert!(destroyed.contains(&"orphan-1".to_string()), "{destroyed:?}");
    assert!(
        p.attempts()
            .iter()
            .all(|id| id == "old-1" || id == "orphan-1"),
        "nothing else is touched: {:?}",
        p.attempts()
    );
    assert!(h.is(
        a,
        &Suspended(SuspendReason::Error("engine restarted".into()))
    ));
}

#[tokio::test(start_paused = true)]
async fn s1_a_failing_leftovers_call_or_destroy_does_not_fail_recovery() {
    // `leftovers` itself fails (docker is down).
    let (h, p) = harness(4);
    p.fail_leftovers("Cannot connect to the Docker daemon");
    h.engine.recover().await.expect("recover is best effort");

    // One destroy fails, the others are still tried.
    let (h, p) = harness(4);
    p.set_leftovers(&["a-1", "b-2", "c-3"]);
    p.fail_destroys_of(&["a-1"]);
    h.engine.recover().await.expect("recover is best effort");
    let mut done = p.destroyed();
    done.sort();
    assert_eq!(done, vec!["b-2".to_string(), "c-3".to_string()]);
}

// 9 ---------------------------------------------------------------------------------------------
// The gateway token never reaches an event.

/// Every event of `runs`, as text, plus the final statuses.
fn everything_recorded(h: &Harness, runs: &[RunId]) -> String {
    let mut all = String::new();
    for run in runs {
        for e in h.store.events_since(*run, 0).unwrap() {
            all.push_str(&format!("{e:?}\n"));
        }
        all.push_str(&format!("{:?}\n", h.status(*run)));
    }
    all
}

fn token_of(h: &Harness, run: RunId) -> String {
    h.store.run_token(run).unwrap().expect("run has a token")
}

#[tokio::test(start_paused = true)]
async fn s1_the_gateway_token_is_in_no_event_across_statuses_and_a_failure() {
    let (mut h, _p) = harness(4);
    // Run 1 fails to provision; the CLI error text echoes the gateway URL the way a daemon
    // error can echo its arguments.
    let failing = h.submit(spec()).await;
    let t1 = token_of(&h, failing);
    h.driver.fail_next_create(&format!(
        "docker: Error response from daemon: invalid env KESTREL_GATEWAY_URL=http://127.0.0.1:7070/r/{t1}"
    ));
    // Run 2: Queued, Provisioning, Running, waiting for approval, Running, then fails.
    let s = h.driver.live();
    h.start();
    let ok = h.submit(spec()).await;
    let t2 = token_of(&h, ok);
    until("first run failed", || h.is_failed(failing).is_some()).await;
    running(&h, ok).await;
    let aid = ApprovalId::new();
    s.line(&format!("approval:{aid}:gh.comment"));
    until("approval", || {
        h.is(ok, &Suspended(SuspendReason::Approval(aid)))
    })
    .await;
    h.control(ok, ControlCommand::Approve(aid));
    running(&h, ok).await;
    s.line("msg:thinking");
    s.line("fail:the agent gave up");
    s.exit(1);
    until("second run failed", || h.is_failed(ok).is_some()).await;

    // The token really was in use, so the test checks something.
    let plans = h.adapters.plans();
    assert!(
        plans
            .iter()
            .flatten()
            .any(|c| c.gateway_url.as_str().contains(&t2)),
        "the run's gateway url carries its token"
    );
    let recorded = everything_recorded(&h, &[failing, ok]);
    assert!(
        !recorded.contains(&t1),
        "token of the failed run leaked:\n{recorded}"
    );
    assert!(
        !recorded.contains(&t2),
        "token of the second run leaked:\n{recorded}"
    );
    assert!(h.is_failed(failing).is_some());
}

/// Stricter than the above: the agent knows its own gateway URL and may print it. The engine is
/// asked to scrub the token from what it records (AgentMessage, AgentLog and the failure text).
#[tokio::test(start_paused = true)]
async fn s1_a_token_echoed_by_the_agent_is_not_recorded() {
    let (mut h, _p) = harness(4);
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    let t = token_of(&h, id);
    running(&h, id).await;
    s.line(&format!(
        "msg:calling http://127.0.0.1:7070/r/{t}/v1/messages"
    ));
    s.line(&format!("GATEWAY=http://127.0.0.1:7070/r/{t}"));
    s.line(&format!("fail:request to /r/{t} failed"));
    s.exit(1);
    until("failed", || h.is_failed(id).is_some()).await;

    let recorded = everything_recorded(&h, &[id]);
    assert!(!recorded.contains(&t), "token leaked:\n{recorded}");
    // The events are still there, only the secret is gone.
    assert!(h.has_event(
        id,
        |k| matches!(k, EventKind::AgentMessage { text } if text.contains("calling"))
    ));
    assert!(logged(&h, id, "GATEWAY="));
}
