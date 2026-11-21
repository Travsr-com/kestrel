//! Issue #38: `Engine::join` (fan-in), the parent's slot while it waits, recursive cancel and
//! no orphans.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::*;
use kestrel_core::caps::RootKey;
use kestrel_core::{ChildResult, JoinError, JoinMode, JoinOutcome};
use kestrel_store::ControlCommand;
use kestrel_types::{AgentSpec, EventKind, MicroUsd, RunId, RunStatus};

const DOLLAR: i64 = 1_000_000;

fn agent(name: &str, keep_children: bool) -> AgentSpec {
    let keep = if keep_children {
        ", keep_children: true"
    } else {
        ""
    };
    let yaml = format!(
        "name: {name}\nruntime: claude-code\nrole: r\n\
         capabilities: {{ mcp: [kestrel.spawn], spawn: {{ max_children: 20, max_depth: 2, agents: [lead, worker]{keep} }} }}\n\
         sandbox: {{ tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
         budget: {{ usd: \"10\", wall_clock: 1h }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture spec parses")
}

fn harness(max: usize) -> Harness {
    let mut h = Harness::new(max);
    let key = RootKey::generate().to_bytes();
    h.rebuild(|e| e.with_root_key(RootKey::from_bytes(&key).unwrap()));
    h
}

/// Starts the engine and a running parent; returns it and its session.
async fn parent(h: &mut Harness, keep_children: bool) -> (RunId, SessionCtl) {
    let session = h.driver.live();
    h.start();
    let id = h.submit(agent("lead", keep_children)).await;
    until("parent running", || h.is(id, &RunStatus::Running)).await;
    (id, session)
}

async fn spawn(h: &Harness, parent: RunId) -> RunId {
    h.engine
        .spawn_child(
            parent,
            &agent("worker", false),
            "help",
            Some(MicroUsd(DOLLAR / 10)),
        )
        .await
        .unwrap()
}

fn outcome(results: &[ChildResult], run: RunId) -> JoinOutcome {
    results
        .iter()
        .find(|r| r.run == run)
        .map(|r| r.outcome.clone())
        .expect("a result for the child")
}

/// Runs `join` in the background, so the test can drive children while it waits.
fn join_in_background(
    h: &Harness,
    parent: RunId,
    children: Vec<RunId>,
    mode: JoinMode,
    timeout: Option<Duration>,
) -> tokio::task::JoinHandle<Result<Vec<ChildResult>, JoinError>> {
    let engine = h.engine.clone();
    tokio::spawn(async move { engine.join(parent, &children, mode, timeout).await })
}

// 1. Cap 1, a parent spawns one child and joins: the child runs and the parent resumes when it
//    finishes. The minimal deadlock case.
async fn scenario_1() {
    let mut h = harness(1);
    let (lead, _s) = parent(&mut h, false).await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead).await;
    let join = join_in_background(&h, lead, vec![child], JoinMode::All, None);
    until("child running", || h.is(child, &RunStatus::Running)).await;
    assert!(
        !h.engine.holds_slot(lead),
        "the waiting parent holds no slot"
    );
    child_session.exit(0);
    let results = join.await.unwrap().unwrap();
    assert!(matches!(
        outcome(&results, child),
        JoinOutcome::Completed { .. }
    ));
    assert!(h.engine.holds_slot(lead), "the parent has its slot back");
    assert!(h.is(lead, &RunStatus::Running));
}

#[tokio::test(start_paused = true)]
async fn s1_cap_one_parent_and_child_do_not_deadlock() {
    scenario_1().await;
}

// 2 and 3. Cap 3, a parent and 5 children, join all: at most 3 slots are held at any moment, the
//    waiting parent holds none, and all 5 finish.
async fn scenario_2() {
    let mut h = harness(3);
    let (lead, _s) = parent(&mut h, false).await;
    let sessions: Vec<SessionCtl> = (0..5).map(|_| h.driver.live()).collect();
    let mut children = Vec::new();
    for _ in 0..5 {
        children.push(spawn(&h, lead).await);
    }
    let join = join_in_background(&h, lead, Vec::new(), JoinMode::All, None);
    let mut peak = 0;
    for session in sessions {
        until("a child running", || {
            peak = peak.max(h.engine.slots_in_use());
            assert!(h.engine.slots_in_use() <= 3, "over the cap");
            children.iter().any(|c| h.is(*c, &RunStatus::Running))
        })
        .await;
        assert!(
            !h.engine.holds_slot(lead),
            "the waiting parent holds no slot"
        );
        session.exit(0);
    }
    let results = join.await.unwrap().unwrap();
    assert_eq!(results.len(), 5);
    for c in &children {
        assert!(matches!(
            outcome(&results, *c),
            JoinOutcome::Completed { .. }
        ));
    }
    assert_eq!(peak, 3, "the children used every slot the parent gave up");
    assert!(h.engine.holds_slot(lead));
}

#[tokio::test(start_paused = true)]
async fn s2_s3_fan_out_over_the_cap_finishes_and_never_overshoots() {
    scenario_2().await;
}

// Scenarios 1 to 3 as a loop-tested race, 200 times each (two tests, so they run in parallel).
#[tokio::test(start_paused = true)]
async fn s1_repeated_200_times() {
    for _ in 0..200 {
        scenario_1().await;
    }
}

#[tokio::test(start_paused = true)]
async fn s2_s3_repeated_200_times() {
    for _ in 0..200 {
        scenario_2().await;
    }
}

// 4. A child that finished before the join returns at once, and the parent keeps its slot.
#[tokio::test(start_paused = true)]
async fn s4_a_child_finished_before_the_join_returns_at_once() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    h.driver.script(&["ok:done the work"], 0);
    let child = spawn(&h, lead).await;
    until("child completed", || h.is(child, &RunStatus::Completed)).await;
    h.store
        .record_model_call(&model_call(child, DOLLAR / 20))
        .unwrap();
    let results = h
        .engine
        .join(lead, &[child], JoinMode::All, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].spent, MicroUsd(DOLLAR / 20));
    assert!(matches!(results[0].outcome, JoinOutcome::Completed { .. }));
    assert!(h.engine.holds_slot(lead));
}

// 5. A child that fails: join returns `Failed(reason)` for it, and the others still return.
#[tokio::test(start_paused = true)]
async fn s5_a_failed_child_is_reported_and_the_others_still_return() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    h.driver.script(&["fail:could not build"], 1);
    let bad = spawn(&h, lead).await;
    h.driver.script(&[], 0);
    let good = spawn(&h, lead).await;
    let results = h
        .engine
        .join(lead, &[bad, good], JoinMode::All, None)
        .await
        .unwrap();
    assert!(
        matches!(outcome(&results, bad), JoinOutcome::Failed { reason } if reason.contains("could not build"))
    );
    assert!(matches!(
        outcome(&results, good),
        JoinOutcome::Completed { .. }
    ));
}

// 6. A timeout shorter than a child's runtime: the finished results plus `TimedOut` for the rest,
//    and the rest are not cancelled.
#[tokio::test(start_paused = true)]
async fn s6_a_timeout_reports_the_unfinished_and_leaves_them_running() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    h.driver.script(&[], 0);
    let quick = spawn(&h, lead).await;
    let _slow_session = h.driver.live();
    let slow = spawn(&h, lead).await;
    until("slow running", || h.is(slow, &RunStatus::Running)).await;
    let results = h
        .engine
        .join(
            lead,
            &[quick, slow],
            JoinMode::All,
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome(&results, quick),
        JoinOutcome::Completed { .. }
    ));
    assert_eq!(outcome(&results, slow), JoinOutcome::TimedOut);
    settle().await;
    assert!(h.is(slow, &RunStatus::Running));
    assert!(h.engine.holds_slot(lead));
}

// 7. `Any` with two children returns when the first finishes.
#[tokio::test(start_paused = true)]
async fn s7_any_returns_when_the_first_child_finishes() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    let first = h.driver.live();
    let a = spawn(&h, lead).await;
    let _second = h.driver.live();
    let b = spawn(&h, lead).await;
    until("both running", || {
        h.is(a, &RunStatus::Running) && h.is(b, &RunStatus::Running)
    })
    .await;
    let join = join_in_background(&h, lead, vec![a, b], JoinMode::Any, None);
    settle().await;
    assert!(!join.is_finished());
    first.exit(0);
    let results = join.await.unwrap().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].run, a);
    assert!(h.is(b, &RunStatus::Running));
}

// 8. A join on a run that is not the caller's child, or an unknown id, fails.
#[tokio::test(start_paused = true)]
async fn s8_only_your_own_children_can_be_joined() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    let stranger = h.submit(spec()).await;
    let err = h
        .engine
        .join(lead, &[stranger], JoinMode::All, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, JoinError::NotYourChild(id) if id == stranger),
        "{err:?}"
    );
    let ghost = RunId::new();
    let err = h
        .engine
        .join(lead, &[ghost], JoinMode::All, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, JoinError::UnknownRun(id) if id == ghost),
        "{err:?}"
    );
    let err = h
        .engine
        .join(ghost, &[], JoinMode::All, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, JoinError::UnknownRun(id) if id == ghost),
        "{err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn joining_with_no_children_returns_nothing_at_once() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    let results = h.engine.join(lead, &[], JoinMode::All, None).await.unwrap();
    assert!(results.is_empty());
}

/// The commit position of a run's `Cancelled` status change in its tree.
fn cancelled_at(h: &Harness, root: RunId, run: RunId) -> u64 {
    h.store
        .events_since_global(root, 0)
        .unwrap()
        .into_iter()
        .find(|(_, e)| {
            e.run_id == run
                && matches!(
                    e.kind,
                    EventKind::StatusChanged {
                        to: RunStatus::Cancelled,
                        ..
                    }
                )
        })
        .map(|(cursor, _)| cursor)
        .expect("a Cancelled status change")
}

// 9. Cancelling a root cancels every descendant, children before their parents, stops every
//    sandbox and records what was spent.
#[tokio::test(start_paused = true)]
async fn s9_cancelling_a_root_cancels_the_whole_tree_children_first() {
    let mut h = harness(8);
    let (root, _s) = parent(&mut h, false).await;
    let _c = h.driver.live();
    let child = h
        .engine
        .spawn_child(root, &agent("lead", false), "mid", Some(MicroUsd(DOLLAR)))
        .await
        .unwrap();
    until("child running", || h.is(child, &RunStatus::Running)).await;
    let _g1 = h.driver.live();
    let grand_a = spawn(&h, child).await;
    let _g2 = h.driver.live();
    let grand_b = spawn(&h, child).await;
    until("grandchildren running", || {
        h.is(grand_a, &RunStatus::Running) && h.is(grand_b, &RunStatus::Running)
    })
    .await;
    h.store
        .record_model_call(&model_call(grand_a, DOLLAR / 50))
        .unwrap();
    h.control(root, ControlCommand::Cancel);
    until("all cancelled", || {
        [root, child, grand_a, grand_b]
            .iter()
            .all(|r| h.is(*r, &RunStatus::Cancelled))
    })
    .await;
    assert!(cancelled_at(&h, root, grand_a) < cancelled_at(&h, root, child));
    assert!(cancelled_at(&h, root, grand_b) < cancelled_at(&h, root, child));
    assert!(cancelled_at(&h, root, child) < cancelled_at(&h, root, root));
    until("sandboxes destroyed", || h.driver.leaked().is_empty()).await;
    assert_eq!(h.store.spent(grand_a).unwrap(), MicroUsd(DOLLAR / 50));
}

// 10. Cancelling a run in the middle cancels its subtree only; the root and siblings keep
//     running, and the root's join sees `Cancelled`.
#[tokio::test(start_paused = true)]
async fn s10_cancelling_mid_tree_leaves_the_root_and_siblings_running() {
    let mut h = harness(8);
    let (root, _s) = parent(&mut h, false).await;
    let _m = h.driver.live();
    let mid = h
        .engine
        .spawn_child(root, &agent("lead", false), "mid", Some(MicroUsd(DOLLAR)))
        .await
        .unwrap();
    let sibling_session = h.driver.live();
    let sibling = spawn(&h, root).await;
    until("mid running", || h.is(mid, &RunStatus::Running)).await;
    let _g = h.driver.live();
    let grand = spawn(&h, mid).await;
    until("grandchild running", || h.is(grand, &RunStatus::Running)).await;
    let join = join_in_background(&h, root, vec![mid, sibling], JoinMode::All, None);
    h.control(mid, ControlCommand::Cancel);
    until("subtree cancelled", || {
        h.is(mid, &RunStatus::Cancelled) && h.is(grand, &RunStatus::Cancelled)
    })
    .await;
    assert!(h.is(root, &RunStatus::Running));
    assert!(h.is(sibling, &RunStatus::Running));
    sibling_session.exit(0);
    let results = join.await.unwrap().unwrap();
    assert_eq!(outcome(&results, mid), JoinOutcome::Cancelled);
    assert!(matches!(
        outcome(&results, sibling),
        JoinOutcome::Completed { .. }
    ));
}

// 11. A parent that completes while a child still runs cancels the child first; the parent's
//     `RunFinished` comes after.
#[tokio::test(start_paused = true)]
async fn s11_a_parent_that_finishes_cancels_its_live_children_first() {
    let mut h = harness(4);
    let (lead, session) = parent(&mut h, false).await;
    let _c = h.driver.live();
    let child = spawn(&h, lead).await;
    until("child running", || h.is(child, &RunStatus::Running)).await;
    session.exit(0);
    until("parent completed", || h.is(lead, &RunStatus::Completed)).await;
    assert!(h.is(child, &RunStatus::Cancelled));
    let finished_at = h
        .store
        .events_since_global(lead, 0)
        .unwrap()
        .into_iter()
        .find(|(_, e)| e.run_id == lead && matches!(e.kind, EventKind::RunFinished { .. }))
        .map(|(c, _)| c)
        .unwrap();
    assert!(cancelled_at(&h, lead, child) < finished_at);
    until("sandboxes destroyed", || h.driver.leaked().is_empty()).await;
}

#[tokio::test(start_paused = true)]
async fn a_parent_that_keeps_its_children_leaves_them_running() {
    let mut h = harness(4);
    let (lead, session) = parent(&mut h, true).await;
    let _c = h.driver.live();
    let child = spawn(&h, lead).await;
    until("child running", || h.is(child, &RunStatus::Running)).await;
    session.exit(0);
    until("parent completed", || h.is(lead, &RunStatus::Completed)).await;
    settle().await;
    assert!(h.is(child, &RunStatus::Running));
}

// 12. A cancel that reaches a child still `Queued` means it never starts and leaves no sandbox.
#[tokio::test(start_paused = true)]
async fn s12_a_queued_child_that_is_cancelled_never_starts() {
    // One slot, held by the parent: the child stays queued.
    let mut h = harness(1);
    let (lead, _s) = parent(&mut h, false).await;
    let child = spawn(&h, lead).await;
    settle().await;
    assert!(h.is(child, &RunStatus::Queued));
    h.control(lead, ControlCommand::Cancel);
    until("both cancelled", || {
        h.is(child, &RunStatus::Cancelled) && h.is(lead, &RunStatus::Cancelled)
    })
    .await;
    assert_eq!(
        h.driver.specs().len(),
        1,
        "only the parent's sandbox was created"
    );
    until("sandboxes destroyed", || h.driver.leaked().is_empty()).await;
}

// 13. After a restart, a join returns the results of children that finished, at once.
#[tokio::test(start_paused = true)]
async fn s13_a_join_after_a_restart_returns_the_right_results() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let mut h = Harness::on(&db, 4);
    h.rebuild(|e| e.with_root_key(RootKey::from_bytes(&key).unwrap()));
    let (lead, _s) = parent(&mut h, false).await;
    h.driver.script(&["ok:all done"], 0);
    let child = spawn(&h, lead).await;
    until("child completed", || h.is(child, &RunStatus::Completed)).await;
    h.shutdown.cancel();
    if let Some(task) = h.task.take() {
        task.await.unwrap().unwrap();
    }
    drop(h);

    let mut again = Harness::on(&db, 4);
    again.rebuild(|e| e.with_root_key(RootKey::from_bytes(&key).unwrap()));
    again.engine.recover().await.unwrap();
    let results = again
        .engine
        .join(lead, &[child], JoinMode::All, None)
        .await
        .unwrap();
    assert_eq!(
        outcome(&results, child),
        JoinOutcome::Completed {
            summary: Some("all done".into())
        }
    );
}

// 14. Two joins on the same child return the same result.
#[tokio::test(start_paused = true)]
async fn s14_two_joins_on_one_child_return_the_same_result() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    let session = h.driver.live();
    let child = spawn(&h, lead).await;
    until("child running", || h.is(child, &RunStatus::Running)).await;
    let one = join_in_background(&h, lead, vec![child], JoinMode::All, None);
    let two = join_in_background(&h, lead, vec![child], JoinMode::All, None);
    settle().await;
    session.line("ok:shared");
    session.exit(0);
    let (one, two) = (one.await.unwrap().unwrap(), two.await.unwrap().unwrap());
    assert_eq!(one, two);
    assert_eq!(
        outcome(&one, child),
        JoinOutcome::Completed {
            summary: Some("shared".into())
        }
    );
    assert!(h.engine.holds_slot(lead));
}

// A join dropped while it waits for its slot back leaves no slot behind.
#[tokio::test(start_paused = true)]
async fn a_join_dropped_while_waiting_for_its_slot_leaks_nothing() {
    let mut h = harness(1);
    let (lead, _s) = parent(&mut h, false).await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead).await;
    let join = join_in_background(&h, lead, vec![child], JoinMode::All, None);
    until("child running", || h.is(child, &RunStatus::Running)).await;
    // Another run queues for the one slot before the child ends, so it gets the slot first and
    // the parent's way back waits behind it.
    let other_session = h.driver.live();
    let other = h.submit(spec()).await;
    settle().await;
    child_session.exit(0);
    until("other running", || h.is(other, &RunStatus::Running)).await;
    assert!(!join.is_finished());
    join.abort();
    other_session.exit(0);
    until("other completed", || h.is(other, &RunStatus::Completed)).await;
    settle().await;
    assert_eq!(h.engine.slots_in_use(), 0, "the dropped join took no slot");
}

#[test]
fn join_errors_read_well() {
    let id = RunId::new();
    for text in [
        JoinError::UnknownRun(id).to_string(),
        JoinError::NotYourChild(id).to_string(),
    ] {
        assert!(text.contains(&id.to_string()));
    }
}
