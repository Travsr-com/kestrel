//! Scenarios 1 to 11 from issue #6: the run engine.
//!
//! Every test runs on a paused clock with a fake driver, a fake adapter and a temp-file store.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::*;
use kestrel_sandbox::NetworkPolicy;
use kestrel_store::ControlCommand;
use kestrel_types::{ApprovalId, EventKind, MicroUsd, RunId, RunStatus, SuspendReason};

use RunStatus::{Cancelled, Completed, Provisioning, Queued, Running, Suspended};

fn warnings(h: &Harness, id: RunId) -> usize {
    h.count_events(id, |k| matches!(k, EventKind::BudgetWarning { .. }))
}

// 1 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s1_happy_path_goes_queued_provisioning_running_completed_with_events_in_order() {
    let mut h = Harness::new(4);
    h.driver
        .script(&["msg:hello", "msg:working on it", "ok:all done"], 0);
    h.start();
    let id = h.submit(spec()).await;

    until("run completed", || h.is(id, &Completed)).await;
    until("sandbox destroyed", || h.driver.leaked().is_empty()).await;

    assert_eq!(
        h.transitions(id),
        vec![
            (Queued, Provisioning),
            (Provisioning, Running),
            (Running, Completed)
        ]
    );
    assert_eq!(
        h.non_status_events(id),
        vec![
            EventKind::RunCreated {
                agent: "tester".into()
            },
            EventKind::SandboxProvisioned {
                driver: "fake".into(),
                sandbox_id: "fake-1".into()
            },
            EventKind::AgentMessage {
                text: "hello".into()
            },
            EventKind::AgentMessage {
                text: "working on it".into()
            },
            EventKind::RunFinished {
                summary: Some("all done".into())
            },
        ]
    );
    assert_eq!(h.driver.count(&Call::Destroy("fake-1".into())), 1);

    // The sandbox is launched from the adapter's plan and the spec's limits, reaching only the
    // gateway.
    let specs = h.driver.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].image, "fake-image");
    assert_eq!(specs[0].argv, vec!["fake-agent", "do the task"]);
    assert_eq!(specs[0].cpu_millis, 1000);
    assert_eq!(specs[0].memory_mb, 512);
    match &specs[0].network {
        NetworkPolicy::GatewayOnly { gateway } => {
            assert!(gateway.as_str().starts_with(GATEWAY), "{gateway}")
        }
        other => panic!("expected GatewayOnly, got {other:?}"),
    }
    // The adapter got a context for this run with a gateway URL under the configured base.
    let plans = h.adapters.plans();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].len(), 1);
    assert_eq!(plans[0][0].run_id, id);
    assert!(plans[0][0].gateway_url.as_str().starts_with(GATEWAY));
}

// 2 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s2_create_failure_fails_run_with_driver_error_and_leaks_nothing() {
    // Cap of 1: if the failed run kept its slot, the second run would never start.
    let mut h = Harness::new(1);
    h.driver.fail_next_create("no space left on device");
    h.driver.script(&["ok:second ran"], 0);
    h.start();
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;

    until("first run failed", || h.is_failed(a).is_some()).await;
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(msg.contains("no space left on device"), "{msg}");
    assert_eq!(h.sandbox_id(a), None);

    until("second run completed", || h.is(b, &Completed)).await;
    until("no sandbox left", || h.driver.leaked().is_empty()).await;
    // Only the one sandbox that exists was ever started.
    assert_eq!(h.driver.starts(), 1);
    assert_eq!(h.driver.count(&Call::Create), 2);
}

// 3 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s3_agent_nonzero_exit_fails_with_adapter_summary() {
    let mut h = Harness::new(4);
    h.driver.script(&["msg:trying", "fail:tests broke"], 1);
    h.start();
    let id = h.submit(spec()).await;

    until("run failed", || h.is_failed(id).is_some()).await;
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains("tests broke"), "{msg}");
    until("sandbox destroyed", || h.driver.leaked().is_empty()).await;
}

// 4 ---------------------------------------------------------------------------------------------

/// Brings a run to `Suspended(Budget)`: 50%, 80% (one warning), then 100%. Returns the run, its
/// sandbox id and the session that is still attached to it.
async fn run_to_budget_suspension(h: &mut Harness) -> (RunId, String, SessionCtl) {
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec_with("1", "1h")).await;
    until("running", || h.is(id, &Running)).await;
    let sid = h.sandbox_id(id).expect("sandbox provisioned");

    // The gateway records model calls straight into the store; agent output keeps flowing.
    h.store.record_model_call(&model_call(id, 500_000)).unwrap();
    s.line("msg:a");
    settle().await;
    assert_eq!(warnings(h, id), 0, "no warning below 80%");
    assert!(h.is(id, &Running));

    h.store.record_model_call(&model_call(id, 300_000)).unwrap();
    s.line("msg:b");
    until("budget warning", || warnings(h, id) == 1).await;
    assert!(h.has_event(id, |k| *k
        == EventKind::BudgetWarning {
            spent: MicroUsd(800_000),
            limit: MicroUsd(1_000_000)
        }));

    // More output and more ticks at 80% give no second warning.
    s.line("msg:c");
    settle().await;
    assert_eq!(warnings(h, id), 1);
    assert!(h.is(id, &Running));

    h.store.record_model_call(&model_call(id, 200_000)).unwrap();
    s.line("msg:d");
    let budget = Suspended(SuspendReason::Budget);
    until("budget suspension", || h.is(id, &budget)).await;
    until("sandbox stopped", || {
        h.driver.count(&Call::Stop(sid.clone())) >= 1
    })
    .await;
    (id, sid, s)
}

#[tokio::test(start_paused = true)]
async fn s4_budget_warns_once_at_80_percent_and_suspends_at_100() {
    let mut h = Harness::new(4);
    let (id, _sid, _s) = run_to_budget_suspension(&mut h).await;

    // Stopping the sandbox does not turn into a failure or a completion.
    settle().await;
    assert!(h.is(id, &Suspended(SuspendReason::Budget)));
    assert_eq!(warnings(&h, id), 1);
}

// 5 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s5_set_budget_then_resume_goes_back_to_running() {
    let mut h = Harness::new(4);
    let (id, sid, _s) = run_to_budget_suspension(&mut h).await;
    let restarted = h.driver.live();

    h.control(id, ControlCommand::SetBudget(MicroUsd(2_000_000)));
    settle().await;
    assert!(
        h.is(id, &Suspended(SuspendReason::Budget)),
        "SetBudget alone does not resume"
    );

    h.control(id, ControlCommand::Resume);
    until("running again", || h.is(id, &Running)).await;
    assert_eq!(
        h.driver.count(&Call::Start(sid.clone())),
        2,
        "the stopped sandbox is started again"
    );

    restarted.line("ok:finished within the new budget");
    restarted.exit(0);
    until("completed", || h.is(id, &Completed)).await;
}

#[tokio::test(start_paused = true)]
async fn s5_resume_without_more_budget_stays_suspended() {
    let mut h = Harness::new(4);
    let (id, _sid, _s) = run_to_budget_suspension(&mut h).await;
    let _restarted = h.driver.live();

    h.control(id, ControlCommand::Resume);
    settle_for(20).await;
    until("still budget-suspended", || {
        h.is(id, &Suspended(SuspendReason::Budget))
    })
    .await;
}

// 6 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s6_cancel_while_provisioning_cancels_and_destroys_sandbox_once_it_exists() {
    let mut h = Harness::new(4);
    let gate = h.driver.gate_create();
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;

    until("provisioning", || {
        h.is(id, &Provisioning) && h.driver.count(&Call::Create) == 1
    })
    .await;
    h.control(id, ControlCommand::Cancel);
    // Control commands are handled while `create` is still blocked.
    until("cancelled", || h.is(id, &Cancelled)).await;

    gate.notify_one();
    until("sandbox destroyed", || {
        h.driver.count(&Call::Destroy("fake-1".into())) == 1
    })
    .await;
    settle().await;
    assert_eq!(
        h.driver.starts(),
        0,
        "a cancelled run never starts its agent"
    );
    assert!(h.is(id, &Cancelled));
    assert!(h.driver.leaked().is_empty());
}

#[tokio::test(start_paused = true)]
async fn s6_cancel_while_running_cancels_and_destroys_sandbox() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;

    h.control(id, ControlCommand::Cancel);
    until("cancelled", || h.is(id, &Cancelled)).await;
    until("sandbox destroyed", || h.driver.leaked().is_empty()).await;
}

// 7 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s7_pause_then_resume_stops_and_restarts_sandbox() {
    let mut h = Harness::new(4);
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;
    let sid = h.sandbox_id(id).expect("sandbox provisioned");
    s.line("msg:before pause");
    until("first message", || {
        h.has_event(
            id,
            |k| matches!(k, EventKind::AgentMessage { text } if text == "before pause"),
        )
    })
    .await;

    h.control(id, ControlCommand::Pause);
    let manual = Suspended(SuspendReason::Manual);
    until("paused", || h.is(id, &manual)).await;
    until("sandbox stopped", || {
        h.driver.count(&Call::Stop(sid.clone())) == 1
    })
    .await;
    // The stopped process exits; that is not the run's outcome.
    settle().await;
    assert!(h.is(id, &manual));

    h.driver.script(&["msg:after resume", "ok:done"], 0);
    h.control(id, ControlCommand::Resume);
    until("completed", || h.is(id, &Completed)).await;
    assert_eq!(h.driver.count(&Call::Start(sid.clone())), 2);

    let t = h.transitions(id);
    assert!(t.contains(&(Running, manual.clone())), "{t:?}");
    assert!(t.contains(&(manual, Running)), "{t:?}");
    assert!(h.has_event(
        id,
        |k| matches!(k, EventKind::AgentMessage { text } if text == "after resume")
    ));
}

// 8 ---------------------------------------------------------------------------------------------

/// Brings a run to `Suspended(Approval(aid))`.
async fn run_to_approval(h: &mut Harness) -> (RunId, ApprovalId, SessionCtl) {
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;
    let aid = ApprovalId::new();
    s.line(&format!("approval:{aid}:gh.comment"));
    until("waiting for approval", || {
        h.is(id, &Suspended(SuspendReason::Approval(aid)))
    })
    .await;
    assert!(h.has_event(id, |k| *k
        == EventKind::ApprovalRequested {
            id: aid,
            action: "gh.comment".into()
        }));
    (id, aid, s)
}

#[tokio::test(start_paused = true)]
async fn s8_approval_requested_suspends_and_approve_resumes() {
    let mut h = Harness::new(4);
    let (id, aid, _s) = run_to_approval(&mut h).await;

    h.control(id, ControlCommand::Approve(aid));
    until("running again", || h.is(id, &Running)).await;
    assert!(h.has_event(id, |k| *k
        == EventKind::ApprovalResolved {
            id: aid,
            approved: true
        }));
}

#[tokio::test(start_paused = true)]
async fn s8_reject_fails_run_with_rejected() {
    let mut h = Harness::new(4);
    let (id, aid, _s) = run_to_approval(&mut h).await;

    h.control(id, ControlCommand::Reject(aid));
    until("failed", || h.is_failed(id).is_some()).await;
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains("rejected"), "{msg}");
    assert!(h.has_event(id, |k| *k
        == EventKind::ApprovalResolved {
            id: aid,
            approved: false
        }));
    until("sandbox destroyed", || h.driver.leaked().is_empty()).await;
}

#[tokio::test(start_paused = true)]
async fn s8_approve_unknown_id_is_noop_with_agent_log_warning() {
    let mut h = Harness::new(4);
    let (id, aid, _s) = run_to_approval(&mut h).await;
    let unknown = ApprovalId::new();

    h.control(id, ControlCommand::Approve(unknown));
    until("warning logged", || {
        h.has_event(
            id,
            |k| matches!(k, EventKind::AgentLog { line } if line.contains(&unknown.to_string())),
        )
    })
    .await;
    settle().await;
    assert!(h.is(id, &Suspended(SuspendReason::Approval(aid))));
    assert!(!h.has_event(
        id,
        |k| matches!(k, EventKind::ApprovalResolved { id, .. } if *id == unknown)
    ));

    // The real approval still works afterwards.
    h.control(id, ControlCommand::Approve(aid));
    until("running again", || h.is(id, &Running)).await;
}

// 9 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s9_restart_recovery_keeps_suspended_and_marks_running_interrupted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("kestrel.db");

    // The first engine accepted three runs, then stopped with one budget-suspended, one running
    // and one still queued.
    let (budget_run, running_run, queued_run) = {
        let first = Harness::on(&db, 4);
        let a = first.submit(spec_with("1", "1h")).await;
        let b = first.submit(spec()).await;
        let c = first.submit(spec()).await;
        let st = &first.store;
        st.transition(a, &Queued, Provisioning).unwrap();
        st.transition(a, &Provisioning, Running).unwrap();
        st.record_model_call(&model_call(a, 1_000_000)).unwrap();
        st.transition(a, &Running, Suspended(SuspendReason::Budget))
            .unwrap();
        st.transition(b, &Queued, Provisioning).unwrap();
        st.append(
            b,
            EventKind::SandboxProvisioned {
                driver: "fake".into(),
                sandbox_id: "gone-1".into(),
            },
        )
        .unwrap();
        st.transition(b, &Provisioning, Running).unwrap();
        (a, b, c)
    };

    let mut h = Harness::on(&db, 4);
    let report = h.engine.recover().await.expect("recover");
    assert_eq!(report.suspended, vec![budget_run]);
    assert_eq!(report.interrupted, vec![running_run]);

    let interrupted = Suspended(SuspendReason::Error("engine restarted".into()));
    assert!(h.is(budget_run, &Suspended(SuspendReason::Budget)));
    assert!(h.is(running_run, &interrupted));
    assert!(h.is(queued_run, &Queued));

    // The new engine schedules the queued run and can resume the budget-suspended one.
    let _queued_session = h.driver.live();
    let _resumed_session = h.driver.live();
    h.start();
    until("queued run started", || h.is(queued_run, &Running)).await;
    h.control(budget_run, ControlCommand::SetBudget(MicroUsd(5_000_000)));
    h.control(budget_run, ControlCommand::Resume);
    until("budget run resumed", || h.is(budget_run, &Running)).await;

    settle().await;
    assert!(
        h.is(running_run, &interrupted),
        "an interrupted run waits for a human"
    );
}

// 10 --------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s10_concurrency_cap_keeps_third_run_queued_until_a_slot_frees() {
    let mut h = Harness::new(2);
    let first = h.driver.live();
    let _second = h.driver.live();
    let _third = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;
    let c = h.submit(spec()).await;

    until("two running", || h.is(a, &Running) && h.is(b, &Running)).await;
    settle_for(20).await;
    assert!(h.is(c, &Queued), "third run waits for a slot");
    assert_eq!(h.driver.count(&Call::Create), 2);

    first.line("ok:done");
    first.exit(0);
    until("third run started", || h.is(c, &Running)).await;
    let completed = [a, b].iter().filter(|r| h.is(**r, &Completed)).count();
    let running = [a, b].iter().filter(|r| h.is(**r, &Running)).count();
    assert_eq!((completed, running), (1, 1));
}

// 11 --------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s11_wall_clock_exceeded_fails_run() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let t0 = tokio::time::Instant::now();
    let id = h.submit(spec_with("1", "60s")).await;
    until("running", || h.is(id, &Running)).await;

    while t0.elapsed() < Duration::from_secs(55) {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle_for(1).await;
    }
    assert!(h.is(id, &Running), "still inside the wall clock limit");

    while t0.elapsed() < Duration::from_secs(65) {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle_for(1).await;
    }
    until("failed on wall clock", || {
        h.is(id, &RunStatus::Failed("wall clock exceeded".into()))
    })
    .await;
    until("sandbox destroyed", || h.driver.leaked().is_empty()).await;
}

// shutdown -------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn run_forever_returns_ok_on_shutdown() {
    let mut h = Harness::new(4);
    h.start();
    settle().await;
    h.shutdown.cancel();
    let task = h.task.take().expect("started");
    let result = task.await.expect("run_forever did not panic");
    assert!(result.is_ok(), "{result:?}");
}
