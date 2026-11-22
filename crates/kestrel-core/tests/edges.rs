//! Engine paths the issue scenarios do not reach: approvals that outlive the agent, recovery of
//! provisioning and approval-suspended runs, commands that do not apply, and runs that cannot
//! start.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::*;
use kestrel_store::ControlCommand;
use kestrel_types::{ApprovalId, EventKind, RunId, RunStatus, SandboxTier, SuspendReason};

use RunStatus::{Cancelled, Completed, Provisioning, Queued, Running, Suspended};

fn logged(h: &Harness, id: RunId, needle: &str) -> bool {
    h.has_event(
        id,
        |k| matches!(k, EventKind::AgentLog { line } if line.contains(needle)),
    )
}

#[tokio::test(start_paused = true)]
async fn agent_that_finishes_while_an_approval_is_pending_completes() {
    let mut h = Harness::new(4);
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;
    let aid = ApprovalId::new();
    s.line(&format!("approval:{aid}:gh.comment"));
    let waiting = Suspended(SuspendReason::Approval(aid));
    until("waiting for approval", || h.is(id, &waiting)).await;

    s.line("ok:did not need it");
    s.exit(0);
    until("completed", || h.is(id, &Completed)).await;
    let t = h.transitions(id);
    assert!(t.contains(&(waiting, Running)), "{t:?}");
    until("sandbox destroyed", || h.driver.leaked().is_empty()).await;
}

#[tokio::test(start_paused = true)]
async fn recovery_suspends_provisioning_runs_and_restarts_approved_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("kestrel.db");
    let aid = ApprovalId::new();
    let (provisioning, approval) = {
        let first = Harness::on(&db, 4);
        let p = first.submit(spec()).await;
        let a = first.submit(spec()).await;
        let st = &first.store;
        st.transition(p, &Queued, Provisioning).unwrap();
        st.transition(a, &Queued, Provisioning).unwrap();
        st.append(
            a,
            EventKind::SandboxProvisioned {
                driver: "fake".into(),
                sandbox_id: "old-1".into(),
            },
        )
        .unwrap();
        st.transition(a, &Provisioning, Running).unwrap();
        st.transition(a, &Running, Suspended(SuspendReason::Approval(aid)))
            .unwrap();
        (p, a)
    };

    // Without automatic resume, so only the approval starts a sandbox here (#92 resumes the
    // provisioning run by default).
    let mut h = Harness::on(&db, 4);
    h.rebuild(|e| e.with_resume_after_restart(false));
    let report = h.engine.recover().await.expect("recover");
    assert_eq!(report.suspended, vec![approval]);
    assert_eq!(report.interrupted, vec![provisioning]);
    assert!(h.is(
        provisioning,
        &Suspended(SuspendReason::Error("engine restarted".into()))
    ));
    assert_eq!(h.driver.count(&Call::Destroy("old-1".into())), 1);

    // Approving after the restart runs the agent again in a fresh sandbox.
    h.driver.script(&["ok:approved and done"], 0);
    h.start();
    h.control(approval, ControlCommand::Approve(aid));
    until("completed", || h.is(approval, &Completed)).await;
    assert_eq!(h.driver.count(&Call::Create), 1);
    assert!(h.has_event(approval, |k| *k
        == EventKind::ApprovalResolved {
            id: aid,
            approved: true,
            payload: None,
        }));
}

#[tokio::test(start_paused = true)]
async fn queued_run_can_be_cancelled_and_ignores_other_commands() {
    let mut h = Harness::new(1);
    let _busy = h.driver.live();
    h.start();
    let first = h.submit(spec()).await;
    until("first running", || h.is(first, &Running)).await;
    let queued = h.submit(spec()).await;

    h.control(queued, ControlCommand::Pause);
    until("pause ignored", || logged(&h, queued, "ignored Pause")).await;
    assert!(h.is(queued, &Queued));

    h.control(queued, ControlCommand::Cancel);
    until("cancelled", || h.is(queued, &Cancelled)).await;

    // A finished run has no actor; commands for it are noted and ignored.
    h.control(queued, ControlCommand::Resume);
    until("resume ignored", || logged(&h, queued, "ignored Resume")).await;
    assert!(h.is(queued, &Cancelled));
    assert_eq!(h.driver.count(&Call::Create), 1);
}

#[tokio::test(start_paused = true)]
async fn commands_that_do_not_apply_to_an_active_run_are_noted() {
    let mut h = Harness::new(4);
    let gate = h.driver.gate_create();
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("provisioning", || h.driver.count(&Call::Create) == 1).await;

    h.control(id, ControlCommand::Pause);
    until("pause ignored", || logged(&h, id, "ignored Pause")).await;
    assert!(h.is(id, &Provisioning));

    gate.notify_one();
    until("running", || h.is(id, &Running)).await;
    h.control(id, ControlCommand::Resume);
    until("resume ignored", || logged(&h, id, "ignored Resume")).await;
    assert!(h.is(id, &Running));
}

#[tokio::test(start_paused = true)]
async fn wall_clock_also_runs_while_provisioning() {
    let mut h = Harness::new(4);
    let gate = h.driver.gate_create();
    h.start();
    let id = h.submit(spec_with("1", "2s")).await;
    until("failed on wall clock", || {
        h.is(id, &RunStatus::Failed("wall clock exceeded".into()))
    })
    .await;

    gate.notify_one();
    until("sandbox destroyed", || {
        h.driver.count(&Call::Destroy("fake-1".into())) == 1
    })
    .await;
    assert_eq!(h.driver.starts(), 0);
}

#[tokio::test(start_paused = true)]
async fn run_fails_when_no_driver_matches_its_tier() {
    let mut h = Harness::new(4);
    h.start();
    let mut gvisor = spec();
    gvisor.sandbox.tier = SandboxTier::Gvisor;
    let id = h.submit(gvisor).await;

    until("failed", || h.is_failed(id).is_some()).await;
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains("no sandbox driver"), "{msg}");
    assert_eq!(h.driver.count(&Call::Create), 0);
}

#[tokio::test(start_paused = true)]
async fn run_without_a_stored_spec_fails() {
    let mut h = Harness::new(4);
    let id = h.submit(spec()).await;
    rusqlite::Connection::open(&h.db)
        .expect("raw open")
        .execute("DELETE FROM run_specs", [])
        .expect("drop spec");
    h.start();

    until("failed", || h.is_failed(id).is_some()).await;
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains("no agent spec"), "{msg}");
    assert_eq!(h.driver.count(&Call::Create), 0);
}
