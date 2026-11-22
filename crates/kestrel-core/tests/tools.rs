//! Issue #42: the engine side of the Kestrel tools a sandbox calls through the gateway: which of
//! them a run may call, `ask_human`, and recording a call that was not permitted.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Harness, until};
use kestrel_core::AskError;
use kestrel_store::ControlCommand;
use kestrel_types::{AgentSpec, ApprovalId, EventKind, RunStatus, SuspendReason};

fn agent(caps: &str) -> AgentSpec {
    agent_for(caps, "1h")
}

fn agent_for(caps: &str, wall_clock: &str) -> AgentSpec {
    let yaml = format!(
        "name: lead\nruntime: claude-code\nrole: r\n\
         capabilities: {caps}\n\
         sandbox: {{ tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
         budget: {{ usd: \"5\", wall_clock: {wall_clock} }}\n"
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

/// Issue #80: a call the gateway refused for lack of room is recorded on the run with the reason.
#[tokio::test(start_paused = true)]
async fn a_tool_call_refused_for_lack_of_room_is_recorded_with_its_reason() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(agent("{ mcp: [kestrel.join] }")).await;
    h.engine
        .tool_refused(id, "kestrel.join", "64 tool calls are already waiting")
        .unwrap();
    assert!(h.has_event(id, |k| matches!(
        k,
        EventKind::AgentLog { line } if line.contains("kestrel.join")
            && line.contains("was refused")
            && line.contains("64 tool calls are already waiting")
    )));
}

// Review 1 of PR 76.

#[tokio::test(start_paused = true)]
async fn the_wall_clock_keeps_going_while_a_question_waits() {
    // The sandbox keeps working while its question waits, so its time counts.
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h
        .submit(agent_for("{ mcp: [kestrel.ask_human] }", "5s"))
        .await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    let engine = h.engine.clone();
    let ask = tokio::spawn(async move { engine.ask_human(id, "Anyone?".into()).await });
    until("asked", || asked(&h, id, "Anyone?").is_some()).await;
    until("out of time", || h.is_failed(id).is_some()).await;
    assert!(h.is_failed(id).unwrap_or_default().contains("wall clock"));
    assert!(matches!(ask.await.unwrap(), Err(AskError::Ended(_))));
}

#[tokio::test(start_paused = true)]
async fn a_question_that_reaches_a_run_that_stopped_says_not_running_not_declined() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    // The pause is queued before the question, so the run has stopped when the question arrives.
    h.control(id, ControlCommand::Pause);
    let err = h
        .engine
        .ask_human(id, "Too late?".into())
        .await
        .unwrap_err();
    assert!(matches!(err, AskError::NotRunning), "{err:?}");
}

#[tokio::test(start_paused = true)]
async fn a_question_still_waiting_at_a_restart_is_declined_and_its_run_failed() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let (asking, approving, question, action) = {
        let first = Harness::on(&db, 4);
        let a = first.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
        let b = first.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
        let (q, act) = (ApprovalId::new(), ApprovalId::new());
        let st = &first.store;
        for (run, id, is_question) in [(a, q, true), (b, act, false)] {
            st.transition(run, &RunStatus::Queued, RunStatus::Provisioning)
                .unwrap();
            st.transition(run, &RunStatus::Provisioning, RunStatus::Running)
                .unwrap();
            st.append(
                run,
                EventKind::ApprovalRequested {
                    id,
                    action: "x".into(),
                    question: is_question,
                },
            )
            .unwrap();
            st.transition(
                run,
                &RunStatus::Running,
                RunStatus::Suspended(SuspendReason::Approval(id)),
            )
            .unwrap();
        }
        (a, b, q, act)
    };
    let h = Harness::on(&db, 4);
    h.engine.recover().await.unwrap();
    // Nobody waits for the answer any more: the question is declined and the run fails, saying why.
    assert!(h.has_event(asking, |k| matches!(
        k,
        EventKind::ApprovalResolved { id, approved: false, payload: Some(p) } if *id == question && p.contains("restarted")
    )));
    assert!(matches!(h.status(asking), RunStatus::Failed(ref m) if m.contains("question")));
    // An action approval still waits for its decision, as before.
    assert!(h.is(
        approving,
        &RunStatus::Suspended(SuspendReason::Approval(action))
    ));
}

// Review 2 of PR 76.

/// A run that the first engine left waiting on question `q`, as if it had stopped there.
fn left_waiting(store: &kestrel_store::Store, run: kestrel_types::RunId, q: ApprovalId) {
    store
        .transition(run, &RunStatus::Queued, RunStatus::Provisioning)
        .unwrap();
    store
        .transition(run, &RunStatus::Provisioning, RunStatus::Running)
        .unwrap();
    store
        .append(
            run,
            EventKind::ApprovalRequested {
                id: q,
                action: "x".into(),
                question: true,
            },
        )
        .unwrap();
    store
        .transition(
            run,
            &RunStatus::Running,
            RunStatus::Suspended(SuspendReason::Approval(q)),
        )
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_run_failed_at_a_restart_for_its_question_leaves_no_children_behind() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let (parent, child) = {
        let first = Harness::on(&db, 4);
        let parent = first.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
        left_waiting(&first.store, parent, ApprovalId::new());
        let child = first
            .store
            .create_child_run(parent, &agent("{ mcp: [] }"), "help")
            .unwrap();
        (parent, child)
    };
    let mut h = Harness::on(&db, 4);
    h.engine.recover().await.unwrap();
    assert!(h.is_failed(parent).is_some());
    let _s = h.driver.live();
    h.start();
    until("the child cancelled", || h.is(child, &RunStatus::Cancelled)).await;
}

#[tokio::test(start_paused = true)]
async fn a_question_answered_just_before_a_restart_is_not_resolved_twice() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let (run, q) = {
        let first = Harness::on(&db, 4);
        let run = first.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
        let q = ApprovalId::new();
        left_waiting(&first.store, run, q);
        // The answer was written; the engine stopped before the run moved on.
        first
            .store
            .append(
                run,
                EventKind::ApprovalResolved {
                    id: q,
                    approved: true,
                    payload: Some("main".into()),
                },
            )
            .unwrap();
        (run, q)
    };
    let h = Harness::on(&db, 4);
    h.engine.recover().await.unwrap();
    assert_eq!(
        h.count_events(
            run,
            |k| matches!(k, EventKind::ApprovalResolved { id, .. } if *id == q)
        ),
        1
    );
    assert!(h.is_failed(run).is_some());
}

#[tokio::test(start_paused = true)]
async fn an_approval_the_agent_asked_for_still_stops_the_clock() {
    // Only the run's own question keeps the clock going; an action approval pauses it as before.
    let mut h = Harness::new(4);
    let s = h.driver.live();
    h.start();
    let id = h
        .submit(agent_for("{ mcp: [kestrel.ask_human] }", "5s"))
        .await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    let aid = ApprovalId::new();
    s.line(&format!("approval:{aid}:gh.comment"));
    until("waiting on the approval", || {
        h.is(id, &RunStatus::Suspended(SuspendReason::Approval(aid)))
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    assert!(h.is(id, &RunStatus::Suspended(SuspendReason::Approval(aid))));
}

// Review 3 of PR 76.

#[tokio::test(start_paused = true)]
async fn a_restart_cancels_a_running_child_and_its_queued_grandchild_of_a_failed_question_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let (child, grandchild) = {
        let first = Harness::on(&db, 4);
        let parent = first.submit(agent("{ mcp: [kestrel.ask_human] }")).await;
        left_waiting(&first.store, parent, ApprovalId::new());
        let st = &first.store;
        let child = st
            .create_child_run(parent, &agent("{ mcp: [] }"), "help")
            .unwrap();
        st.transition(child, &RunStatus::Queued, RunStatus::Provisioning)
            .unwrap();
        st.transition(child, &RunStatus::Provisioning, RunStatus::Running)
            .unwrap();
        let grandchild = st
            .create_child_run(child, &agent("{ mcp: [] }"), "more")
            .unwrap();
        (child, grandchild)
    };
    let mut h = Harness::on(&db, 4);
    h.engine.recover().await.unwrap();
    let _s = h.driver.live();
    h.start();
    until("the child cancelled", || h.is(child, &RunStatus::Cancelled)).await;
    until("the grandchild cancelled", || {
        h.is(grandchild, &RunStatus::Cancelled)
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_run_that_keeps_its_children_keeps_them_after_a_restart_too() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let child = {
        let first = Harness::on(&db, 4);
        let parent = first
            .submit(agent(
                "{ mcp: [kestrel.ask_human], spawn: { max_children: 2, max_depth: 1, agents: [lead], keep_children: true } }",
            ))
            .await;
        left_waiting(&first.store, parent, ApprovalId::new());
        first
            .store
            .create_child_run(parent, &agent("{ mcp: [] }"), "help")
            .unwrap()
    };
    let mut h = Harness::on(&db, 4);
    h.engine.recover().await.unwrap();
    let _s = h.driver.live();
    h.start();
    until("the child runs", || h.is(child, &RunStatus::Running)).await;
}

/// Issue #95: `kestrel.messages` is offered to a run whose capability grants it, after the other
/// tools, and hands over a running run's messages once, recording their delivery.
#[tokio::test(start_paused = true)]
async fn messages_are_offered_when_granted_and_handed_over_once() {
    let mut h = Harness::new(4);
    let _a = h.driver.live();
    let _b = h.driver.live();
    h.start();
    let a = h
        .submit(agent("{ mcp: [kestrel.messages, kestrel.join] }"))
        .await;
    let b = h.submit(agent("{ mcp: [kestrel.join] }")).await;
    for run in [a, b] {
        until("running", || h.is(run, &RunStatus::Running)).await;
    }
    assert_eq!(
        h.engine.kestrel_tools(a).unwrap(),
        Some(vec!["kestrel.join", "kestrel.messages"])
    );
    assert_eq!(
        h.engine.kestrel_tools(b).unwrap(),
        Some(vec!["kestrel.join"])
    );

    assert!(h.engine.take_messages(a, None).unwrap().is_empty());
    let sent = h.store.send_message(a, "also fix the docs").unwrap();
    let taken = h.engine.take_messages(a, None).unwrap();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].seq, sent.seq);
    assert_eq!(taken[0].text, "also fix the docs");
    assert!(h.engine.take_messages(a, None).unwrap().is_empty());
    // Asked again from before it (a retry with `after`), it comes again, and is not delivered
    // twice.
    let again = h.engine.take_messages(a, Some(sent.seq - 1)).unwrap();
    assert_eq!(again, taken);
    assert_eq!(
        h.count_events(
            a,
            |k| matches!(k, EventKind::MessageDelivered { seqs } if *seqs == [sent.seq])
        ),
        1
    );
    // The engine never logs a message's text anywhere but its own event.
    assert_eq!(
        h.count_events(
            a,
            |k| matches!(k, EventKind::AgentLog { line } if line.contains("also fix the docs"))
        ),
        0
    );
}
