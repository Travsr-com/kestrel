//! Issue #18, more on the line for a slot: duplicate resumes, a queued run cancelled while it
//! waits, and a resume that is declined after it was given a slot.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::*;
use kestrel_store::ControlCommand;
use kestrel_types::{EventKind, RunStatus, SuspendReason};

use RunStatus::{Cancelled, Completed, Queued, Running, Suspended};

const MANUAL: RunStatus = Suspended(SuspendReason::Manual);
const BUDGET: RunStatus = Suspended(SuspendReason::Budget);

fn said(line: &str) -> impl Fn(&EventKind) -> bool + '_ {
    move |e| matches!(e, EventKind::AgentLog { line: l } if l.contains(line))
}

#[tokio::test(start_paused = true)]
async fn a_second_resume_while_one_waits_is_noted_and_the_run_resumes_once() {
    let mut h = Harness::new(1);
    let _s_s = h.driver.live();
    let s_a = h.driver.live();
    let _s_s2 = h.driver.live();
    h.start();
    let s = h.submit(spec()).await;
    until("s running", || h.is(s, &Running)).await;
    h.control(s, ControlCommand::Pause);
    until("s paused", || h.is(s, &MANUAL)).await;
    let a = h.submit(spec()).await;
    until("a running", || h.is(a, &Running)).await;

    h.control(s, ControlCommand::Resume);
    h.control(s, ControlCommand::Resume);
    until("the duplicate is noted", || {
        h.has_event(s, said("already waits for a slot"))
    })
    .await;
    assert!(h.is(s, &MANUAL));

    s_a.line("ok:done");
    s_a.exit(0);
    until("s running again", || h.is(s, &Running)).await;
    assert_eq!(h.driver.starts(), 3, "one start for s, not two");
}

#[tokio::test(start_paused = true)]
async fn a_queued_run_cancelled_while_it_waits_never_starts_and_takes_no_slot() {
    let mut h = Harness::new(1);
    let s_a = h.driver.live();
    let _s_c = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    until("a running", || h.is(a, &Running)).await;
    let b = h.submit(spec()).await;
    settle_for(5).await;
    assert!(h.is(b, &Queued));
    h.control(b, ControlCommand::Cancel);
    until("b cancelled", || h.is(b, &Cancelled)).await;

    let c = h.submit(spec()).await;
    s_a.line("ok:done");
    s_a.exit(0);
    until("c running", || h.is(c, &Running)).await;
    assert!(h.is(b, &Cancelled));
    assert_eq!(h.driver.starts(), 2, "b never started");
}

#[tokio::test(start_paused = true)]
async fn a_declined_resume_gives_its_slot_back() {
    let mut h = Harness::new(1);
    let s_s = h.driver.live();
    let _s_b = h.driver.live();
    h.start();
    let s = h.submit(spec()).await;
    until("s running", || h.is(s, &Running)).await;
    h.store
        .record_model_call(&model_call(s, 1_000_000))
        .unwrap();
    s_s.line("msg:spent");
    until("s over budget", || h.is(s, &BUDGET)).await;

    // The slot is free, but the run is still over its budget, so the resume is declined.
    h.control(s, ControlCommand::Resume);
    until("declined", || h.has_event(s, said("still over its budget"))).await;
    let b = h.submit(spec()).await;
    until("b running", || h.is(b, &Running)).await;
    assert!(h.is(s, &BUDGET));
    assert_ne!(h.status(b), Completed);
}
