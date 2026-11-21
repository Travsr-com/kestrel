//! Issue #42: the engine side of the Kestrel tools a sandbox calls through the gateway: which of
//! them a run may call, `ask_human`, and recording a call that was not permitted.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Harness, until};
use kestrel_core::AskError;
use kestrel_store::ControlCommand;
use kestrel_types::{AgentSpec, ApprovalId, EventKind, RunStatus, SuspendReason};

fn agent(caps: &str) -> AgentSpec {
    let yaml = format!(
        "name: lead\nruntime: claude-code\nrole: r\n\
         capabilities: {caps}\n\
         sandbox: {{ tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
         budget: {{ usd: \"5\", wall_clock: 1h }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture spec parses")
}

/// The id of the question the run asked, once it is in the log.
fn asked(h: &Harness, run: kestrel_types::RunId, question: &str) -> Option<ApprovalId> {
    h.events(run).into_iter().find_map(|k| match k {
        EventKind::ApprovalRequested {
            id,
            action,
            question: true,
        } if action == question => Some(id),
        _ => None,
    })
}

#[tokio::test(start_paused = true)]
async fn the_kestrel_tools_a_run_may_call_follow_its_capabilities() {
    let mut h = Harness::new(4);
    let _a = h.driver.live();
    let _b = h.driver.live();
    let _c = h.driver.live();
    h.start();
    let a = h
        .submit(agent("{ mcp: [kestrel.join, kestrel.ask_human, fs.read] }"))
        .await;
    let b = h
        .submit(agent(
            "{ mcp: [kestrel.spawn, kestrel.join], spawn: { max_children: 2, max_depth: 1, agents: [worker] } }",
        ))
        .await;
    // Listed, but with no spawn rights it cannot be called.
    let c = h.submit(agent("{ mcp: [kestrel.spawn] }")).await;
    for run in [a, b, c] {
        until("running", || h.is(run, &RunStatus::Running)).await;
    }
    assert_eq!(
        h.engine.kestrel_tools(a).unwrap(),
        Some(vec!["kestrel.join", "kestrel.ask_human"])
    );
    assert_eq!(
        h.engine.kestrel_tools(b).unwrap(),
        Some(vec!["kestrel.spawn", "kestrel.join"])
    );
    assert_eq!(h.engine.kestrel_tools(c).unwrap(), Some(vec![]));
    h.control(a, ControlCommand::Cancel);
    until("cancelled", || h.is(a, &RunStatus::Cancelled)).await;
    assert_eq!(h.engine.kestrel_tools(a).unwrap(), None);
    assert!(
        h.engine
            .kestrel_tools(kestrel_types::RunId::new())
            .unwrap()
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn ask_human_suspends_the_run_and_returns_the_answer() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    let engine = h.engine.clone();
    let ask = tokio::spawn(async move { engine.ask_human(id, "Which branch?".into()).await });
    until("asked", || asked(&h, id, "Which branch?").is_some()).await;
    let aid = asked(&h, id, "Which branch?").unwrap();
    until("waiting on the answer", || {
        h.is(id, &RunStatus::Suspended(SuspendReason::Approval(aid)))
    })
    .await;
    h.control(
        id,
        ControlCommand::ApproveWith {
            id: aid,
            payload: "release/2.4".into(),
        },
    );
    assert_eq!(ask.await.unwrap().unwrap(), "release/2.4");
    until("running again", || h.is(id, &RunStatus::Running)).await;
}

#[tokio::test(start_paused = true)]
async fn a_declined_question_says_why() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    let engine = h.engine.clone();
    let ask = tokio::spawn(async move { engine.ask_human(id, "Ship it?".into()).await });
    until("asked", || asked(&h, id, "Ship it?").is_some()).await;
    let aid = asked(&h, id, "Ship it?").unwrap();
    h.control(
        id,
        ControlCommand::RejectWith {
            id: aid,
            reason: "not before the review".into(),
        },
    );
    let err = ask.await.unwrap().unwrap_err();
    assert!(
        matches!(err, AskError::Declined(Some(ref r)) if r == "not before the review"),
        "{err:?}"
    );
    assert!(err.to_string().contains("not before the review"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn a_question_from_a_run_that_is_not_running_is_refused() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    h.control(id, ControlCommand::Pause);
    until("paused", || {
        h.is(id, &RunStatus::Suspended(SuspendReason::Manual))
    })
    .await;
    let err = h.engine.ask_human(id, "Anyone?".into()).await.unwrap_err();
    assert!(matches!(err, AskError::NotRunning), "{err:?}");
    assert!(matches!(
        h.engine
            .ask_human(kestrel_types::RunId::new(), "x".into())
            .await,
        Err(AskError::NotRunning)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_run_that_ends_while_its_question_waits_ends_the_wait() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    let engine = h.engine.clone();
    let ask = tokio::spawn(async move { engine.ask_human(id, "Still there?".into()).await });
    until("asked", || asked(&h, id, "Still there?").is_some()).await;
    h.control(id, ControlCommand::Cancel);
    let err = ask.await.unwrap().unwrap_err();
    assert!(
        matches!(err, AskError::Ended(RunStatus::Cancelled)),
        "{err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_question_that_reaches_a_run_no_longer_running_is_answered_with_a_refusal() {
    // The command is handled by the run itself, which may have stopped running meanwhile.
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    h.control(id, ControlCommand::Pause);
    let aid = ApprovalId::new();
    h.control(
        id,
        ControlCommand::Ask {
            id: aid,
            question: "late".into(),
        },
    );
    until("refused", || {
        h.has_event(
            id,
            |k| matches!(k, EventKind::ApprovalResolved { id, approved: false, .. } if *id == aid),
        )
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_tool_call_that_was_not_permitted_is_recorded() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(agent("{ mcp: [kestrel.join] }")).await;
    h.engine.tool_denied(id, "kestrel.spawn").unwrap();
    assert!(h.has_event(id, |k| matches!(
        k,
        EventKind::AgentLog { line } if line.contains("kestrel.spawn") && line.contains("not permitted")
    )));
}
