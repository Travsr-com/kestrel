//! Issue #18: a run holds a concurrency slot only while its sandbox runs or is being provisioned.
//!
//! Holding states: `Provisioning`, `Running` and `Suspended(Approval)` (the sandbox keeps running
//! while an approval is pending). `Suspended(Budget | Manual | Error)` has a stopped sandbox and
//! holds no slot. Resuming such a run needs a free slot first; until then the resume stays
//! pending and the run stays suspended. Pending resumes and `Queued` runs share one FIFO line.
//!
//! The accounting type itself (`Slots`) is unit-tested in `src/slots.rs`. Everything here runs
//! on a paused clock with the fake driver, the fake adapter and a temp-file store.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::*;
use kestrel_store::ControlCommand;
use kestrel_types::{ApprovalId, MicroUsd, RunId, RunStatus, SuspendReason};

use RunStatus::{Cancelled, Completed, Failed, Provisioning, Queued, Running, Suspended};

const BUDGET: RunStatus = Suspended(SuspendReason::Budget);
const MANUAL: RunStatus = Suspended(SuspendReason::Manual);

/// True while the run's sandbox is running or being provisioned.
fn holds_slot(h: &Harness, id: RunId) -> bool {
    matches!(
        h.status(id),
        Provisioning | Running | Suspended(SuspendReason::Approval(_))
    )
}

fn held(h: &Harness, ids: &[RunId]) -> usize {
    ids.iter().filter(|id| holds_slot(h, **id)).count()
}

/// Like `until`, and also fails the moment more than `cap` of `ids` hold a slot.
async fn until_capped(
    what: &str,
    h: &Harness,
    ids: &[RunId],
    cap: usize,
    mut cond: impl FnMut() -> bool,
) {
    until(what, || {
        let n = held(h, ids);
        assert!(
            n <= cap,
            "{n} runs hold a slot, cap is {cap}, while waiting for {what}"
        );
        cond()
    })
    .await;
}

/// Lets the engine run for `ticks` poll intervals, checking the cap after every step.
async fn settle_capped(h: &Harness, ids: &[RunId], cap: usize, ticks: u32) {
    for _ in 0..ticks {
        settle_for(1).await;
        let n = held(h, ids);
        assert!(n <= cap, "{n} runs hold a slot, cap is {cap}");
    }
}

/// The agent reports success and exits.
fn finish(s: SessionCtl) {
    s.line("ok:done");
    s.exit(0);
}

/// Spends the whole $1 budget of a running run and waits until its sandbox is stopped.
async fn suspend_budget(h: &Harness, id: RunId, s: &SessionCtl) {
    h.store
        .record_model_call(&model_call(id, 1_000_000))
        .unwrap();
    s.line("msg:spent");
    until("budget suspension", || h.is(id, &BUDGET)).await;
    stopped(h, id).await;
}

/// Pauses a running run and waits until its sandbox is stopped.
async fn pause(h: &Harness, id: RunId) {
    h.control(id, ControlCommand::Pause);
    until("manual suspension", || h.is(id, &MANUAL)).await;
    stopped(h, id).await;
}

async fn stopped(h: &Harness, id: RunId) {
    let sid = h.sandbox_id(id).expect("sandbox provisioned");
    until("sandbox stopped", || {
        h.driver.count(&Call::Stop(sid.clone())) >= 1
    })
    .await;
}

async fn running(h: &Harness, id: RunId) {
    until("running", || h.is(id, &Running)).await;
}

// 1 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s1_a_budget_suspended_run_frees_its_slot_so_two_more_runs_start_under_cap_two() {
    let mut h = Harness::new(2);
    let s_susp = h.driver.live();
    let _sb = h.driver.live();
    let _sc = h.driver.live();
    h.start();
    let a = h.submit(spec_with("1", "1h")).await;
    running(&h, a).await;
    suspend_budget(&h, a, &s_susp).await;

    let b = h.submit(spec()).await;
    let c = h.submit(spec()).await;
    until("both new runs running", || {
        h.is(b, &Running) && h.is(c, &Running)
    })
    .await;
    assert!(h.is(a, &BUDGET));
    assert_eq!(h.driver.count(&Call::Create), 3);
}

#[tokio::test(start_paused = true)]
async fn s1_a_manually_paused_run_frees_its_slot() {
    let mut h = Harness::new(1);
    let _sa = h.driver.live();
    let _sb = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    pause(&h, a).await;

    let b = h.submit(spec()).await;
    until("second run running", || h.is(b, &Running)).await;
    assert!(h.is(a, &MANUAL));
}

// 2 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s2_a_resume_past_the_cap_waits_for_a_slot_then_runs() {
    let mut h = Harness::new(2);
    let s_c = h.driver.live();
    let s_a = h.driver.live();
    let _s_b = h.driver.live();
    let s_c2 = h.driver.live();
    h.start();
    let c = h.submit(spec_with("1", "1h")).await;
    running(&h, c).await;
    suspend_budget(&h, c, &s_c).await;
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;
    until("a and b running", || h.is(a, &Running) && h.is(b, &Running)).await;
    let all = [a, b, c];

    // More budget, then resume: the cap is full, so the resume waits and is not lost.
    h.control(c, ControlCommand::SetBudget(MicroUsd(2_000_000)));
    h.control(c, ControlCommand::Resume);
    settle_capped(&h, &all, 2, 20).await;
    assert!(h.is(c, &BUDGET), "resume waits in line");
    assert_eq!(
        h.driver.starts(),
        3,
        "no fourth start while the cap is full"
    );

    finish(s_a);
    until_capped("c resumed into the freed slot", &h, &all, 2, || {
        h.is(c, &Running)
    })
    .await;
    assert!(h.is(a, &Completed));
    assert!(h.is(b, &Running));

    finish(s_c2);
    until("c completed", || h.is(c, &Completed)).await;
}

#[tokio::test(start_paused = true)]
async fn s2_a_pending_resume_can_still_be_cancelled_and_never_takes_a_slot() {
    let mut h = Harness::new(1);
    let _s_s = h.driver.live();
    let s_a = h.driver.live();
    let _s_b = h.driver.live();
    h.start();
    let s = h.submit(spec()).await;
    running(&h, s).await;
    pause(&h, s).await;
    let a = h.submit(spec()).await;
    running(&h, a).await;

    h.control(s, ControlCommand::Resume);
    settle_for(5).await;
    assert!(h.is(s, &MANUAL));
    h.control(s, ControlCommand::Cancel);
    until("cancelled", || h.is(s, &Cancelled)).await;

    let b = h.submit(spec()).await;
    settle_capped(&h, &[a, b, s], 1, 10).await;
    assert!(h.is(b, &Queued));
    finish(s_a);
    until("b running", || h.is(b, &Running)).await;
    assert_eq!(
        h.driver.starts(),
        3,
        "the cancelled run never started again"
    );
}

// 3 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s3_a_run_waiting_for_approval_keeps_its_slot() {
    let mut h = Harness::new(1);
    let s_a = h.driver.live();
    let s_b = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let aid = ApprovalId::new();
    s_a.line(&format!("approval:{aid}:gh.comment"));
    until("waiting for approval", || {
        h.is(a, &Suspended(SuspendReason::Approval(aid)))
    })
    .await;

    let b = h.submit(spec()).await;
    settle_capped(&h, &[a, b], 1, 20).await;
    assert!(
        h.is(b, &Queued),
        "the approval wait still holds the only slot"
    );
    assert_eq!(h.driver.count(&Call::Create), 1);
    let sid = h.sandbox_id(a).expect("sandbox");
    assert_eq!(
        h.driver.count(&Call::Stop(sid)),
        0,
        "the sandbox keeps running"
    );

    // Approving needs no new slot; when the run ends the slot passes to b.
    h.control(a, ControlCommand::Approve(aid));
    until("a running again", || h.is(a, &Running)).await;
    assert!(h.is(b, &Queued));
    s_a.line("ok:done");
    s_a.exit(0);
    until("b running", || h.is(b, &Running)).await;
    finish(s_b);
    until("b completed", || h.is(b, &Completed)).await;
}

// 4 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s4_after_a_restart_suspended_runs_hold_no_slot_and_commands_respect_the_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("kestrel.db");
    let aid = ApprovalId::new();
    let error = Suspended(SuspendReason::Error("engine restarted".into()));
    let (r1, r2, r3) = {
        let first = Harness::on(&db, 1);
        let r1 = first.submit(spec()).await;
        let r2 = first.submit(spec()).await;
        let r3 = first.submit(spec()).await;
        let st = &first.store;
        for (id, to) in [
            (r1, MANUAL),
            (r2, error.clone()),
            (r3, Suspended(SuspendReason::Approval(aid))),
        ] {
            st.transition(id, &Queued, Provisioning).unwrap();
            st.transition(id, &Provisioning, Running).unwrap();
            st.transition(id, &Running, to).unwrap();
        }
        (r1, r2, r3)
    };

    let mut h = Harness::on(&db, 1);
    let report = h.engine.recover().await.expect("recover");
    assert_eq!(report.suspended, vec![r1, r2, r3]);
    let s_q = h.driver.live();
    let s1 = h.driver.live();
    let s2 = h.driver.live();
    let s3 = h.driver.live();
    h.start();

    // The suspended runs hold no slot, so a fresh run takes the only one.
    let q = h.submit(spec()).await;
    until("new run running", || h.is(q, &Running)).await;
    // r3 is approval-suspended, but its sandbox died with the old engine: it holds no slot.
    let all = [q, r1, r2];

    h.control(r1, ControlCommand::Resume);
    h.control(r2, ControlCommand::Resume);
    h.control(r3, ControlCommand::Approve(aid));
    settle_capped(&h, &all, 1, 20).await;
    assert!(h.is(r1, &MANUAL), "no actor starts past the cap");
    assert!(h.is(r2, &error));
    assert!(h.is(r3, &Suspended(SuspendReason::Approval(aid))));
    assert_eq!(h.driver.count(&Call::Create), 1);

    // Commands are served one slot at a time, in the order they arrived.
    finish(s_q);
    until_capped("r1 resumed", &h, &all, 1, || h.is(r1, &Running)).await;
    assert!(h.is(r2, &error) && h.is(r3, &Suspended(SuspendReason::Approval(aid))));
    finish(s1);
    until_capped("r2 resumed", &h, &all, 1, || h.is(r2, &Running)).await;
    assert!(h.is(r3, &Suspended(SuspendReason::Approval(aid))));
    finish(s2);
    // r3 is left out of the capped wait: until it resumes it is `Suspended(Approval)`, which the
    // status-based `holds_slot` counts as holding, although after the restart its sandbox is gone
    // and it holds nothing (see the comment above). Once it is running it must hold the only slot.
    until_capped("r3 resumed", &h, &all, 1, || h.is(r3, &Running)).await;
    assert_eq!(
        held(&h, &[q, r1, r2, r3]),
        1,
        "once r3 runs, it holds the only slot"
    );
    finish(s3);
    until("all completed", || {
        all.iter().all(|id| h.is(*id, &Completed))
    })
    .await;
}

// 5 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s5_a_pending_resume_does_not_jump_ahead_of_an_earlier_queued_run() {
    let mut h = Harness::new(1);
    let _s_s = h.driver.live();
    let s_a = h.driver.live();
    let s_q = h.driver.live();
    let s_s2 = h.driver.live();
    h.start();
    let s = h.submit(spec()).await;
    running(&h, s).await;
    pause(&h, s).await;
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let q = h.submit(spec()).await;
    settle_for(5).await;
    assert!(h.is(q, &Queued));
    let all = [s, a, q];

    // The resume arrives after q was queued, so q goes first.
    h.control(s, ControlCommand::Resume);
    settle_capped(&h, &all, 1, 5).await;
    finish(s_a);
    until_capped("q runs first", &h, &all, 1, || h.is(q, &Running)).await;
    assert!(h.is(s, &MANUAL), "the resume is still waiting");
    finish(s_q);
    until_capped("s resumed", &h, &all, 1, || h.is(s, &Running)).await;
    finish(s_s2);
    until("s completed", || h.is(s, &Completed)).await;
}

#[tokio::test(start_paused = true)]
async fn s5_a_queued_run_does_not_jump_ahead_of_an_earlier_pending_resume() {
    let mut h = Harness::new(1);
    let _s_s = h.driver.live();
    let s_a = h.driver.live();
    let s_s2 = h.driver.live();
    let s_q = h.driver.live();
    h.start();
    let s = h.submit(spec()).await;
    running(&h, s).await;
    pause(&h, s).await;
    let a = h.submit(spec()).await;
    running(&h, a).await;

    // The resume arrives first, then q is submitted.
    h.control(s, ControlCommand::Resume);
    settle_for(5).await;
    let q = h.submit(spec()).await;
    settle_for(5).await;
    let all = [s, a, q];
    assert!(h.is(s, &MANUAL) && h.is(q, &Queued));

    finish(s_a);
    until_capped("s resumes first", &h, &all, 1, || h.is(s, &Running)).await;
    assert!(h.is(q, &Queued), "q waits behind the resume");
    finish(s_s2);
    until_capped("q runs", &h, &all, 1, || h.is(q, &Running)).await;
    finish(s_q);
    until("q completed", || h.is(q, &Completed)).await;
}

// 6 ---------------------------------------------------------------------------------------------

/// The M2 deadlock: cap 2, two parents, each waiting on one queued child. A parent parked on its
/// children is a suspended run with a stopped sandbox (here `Suspended(Budget)`); it must free its
/// slot, or the children never start. The `Slots`-level version is in `src/slots.rs`.
#[tokio::test(start_paused = true)]
async fn s6_two_parents_waiting_on_queued_children_under_cap_two_everything_finishes() {
    let mut h = Harness::new(2);
    let s_p1 = h.driver.live();
    let s_p2 = h.driver.live();
    let s_c1 = h.driver.live();
    let s_c2 = h.driver.live();
    let s_p1b = h.driver.live();
    let s_p2b = h.driver.live();
    h.start();
    let p1 = h.submit(spec_with("1", "1h")).await;
    let p2 = h.submit(spec_with("1", "1h")).await;
    until("parents running", || {
        h.is(p1, &Running) && h.is(p2, &Running)
    })
    .await;

    // Each parent spawns one child. Both children queue: the cap is full of parents.
    let c1 = h.submit(spec()).await;
    let c2 = h.submit(spec()).await;
    let all = [p1, p2, c1, c2];
    settle_capped(&h, &all, 2, 10).await;
    assert!(h.is(c1, &Queued) && h.is(c2, &Queued));

    // The parents park, waiting for their children.
    suspend_budget(&h, p1, &s_p1).await;
    suspend_budget(&h, p2, &s_p2).await;
    until_capped("children running", &h, &all, 2, || {
        h.is(c1, &Running) && h.is(c2, &Running)
    })
    .await;

    // The children finish; the parents wake up with their results and finish too.
    finish(s_c1);
    finish(s_c2);
    until("children completed", || {
        h.is(c1, &Completed) && h.is(c2, &Completed)
    })
    .await;
    for p in [p1, p2] {
        h.control(p, ControlCommand::SetBudget(MicroUsd(2_000_000)));
        h.control(p, ControlCommand::Resume);
    }
    until_capped("parents running again", &h, &all, 2, || {
        h.is(p1, &Running) && h.is(p2, &Running)
    })
    .await;
    finish(s_p1b);
    finish(s_p2b);
    until("everything completed", || {
        all.iter().all(|id| h.is(*id, &Completed))
    })
    .await;
}

// 7 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s7_completed_failed_and_cancelled_runs_each_release_their_slot_exactly_once() {
    let mut h = Harness::new(1);
    let s_a = h.driver.live();
    let s_b = h.driver.live();
    let _s_c = h.driver.live();
    let _s_d = h.driver.live();
    let _s_e = h.driver.live();
    h.start();
    let ids = [
        h.submit(spec()).await,
        h.submit(spec()).await,
        h.submit(spec()).await,
        h.submit(spec()).await,
        h.submit(spec()).await,
    ];
    let [a, b, c, d, e] = ids;

    until_capped("a running", &h, &ids, 1, || h.is(a, &Running)).await;
    settle_capped(&h, &ids, 1, 10).await;
    finish(s_a);
    until_capped("b running", &h, &ids, 1, || h.is(b, &Running)).await;
    assert!(h.is(a, &Completed));

    s_b.line("fail:nope");
    s_b.exit(1);
    until_capped("c running", &h, &ids, 1, || h.is(c, &Running)).await;
    assert!(matches!(h.status(b), Failed(_)));

    h.control(c, ControlCommand::Cancel);
    until_capped("d running", &h, &ids, 1, || h.is(d, &Running)).await;
    assert!(h.is(c, &Cancelled));

    // A double release would have let e start beside d.
    settle_capped(&h, &ids, 1, 20).await;
    assert!(h.is(e, &Queued));
}

#[tokio::test(start_paused = true)]
async fn s7_cancelling_a_suspended_run_does_not_free_a_slot_it_does_not_hold() {
    let mut h = Harness::new(1);
    let _s_s = h.driver.live();
    let s_a = h.driver.live();
    let _s_b = h.driver.live();
    h.start();
    let s = h.submit(spec()).await;
    running(&h, s).await;
    pause(&h, s).await;
    let a = h.submit(spec()).await;
    running(&h, a).await;

    h.control(s, ControlCommand::Cancel);
    until("cancelled", || h.is(s, &Cancelled)).await;
    let b = h.submit(spec()).await;
    let all = [s, a, b];
    settle_capped(&h, &all, 1, 20).await;
    assert!(h.is(b, &Queued), "a still holds the only slot");

    finish(s_a);
    until_capped("b running", &h, &all, 1, || h.is(b, &Running)).await;
}

#[tokio::test(start_paused = true)]
async fn s7_the_exit_of_a_stopped_sandbox_does_not_release_the_slot_a_second_time() {
    let mut h = Harness::new(1);
    let s_a = h.driver.live();
    let _s_b = h.driver.live();
    let _s_c = h.driver.live();
    h.start();
    let a = h.submit(spec_with("1", "1h")).await;
    running(&h, a).await;
    suspend_budget(&h, a, &s_a).await;
    let b = h.submit(spec()).await;
    until("b running", || h.is(b, &Running)).await;
    let c = h.submit(spec()).await;
    let all = [a, b, c];

    // The stopped sandbox's process has exited by now; that must not free b's slot.
    settle_capped(&h, &all, 1, 20).await;
    assert!(h.is(c, &Queued));
    assert!(h.is(a, &BUDGET));
}
