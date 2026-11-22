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

/// A small deterministic generator, so each loop iteration takes a different, repeatable path.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % below.max(1)
    }
}

// 2 and 3. Cap 3, a parent and 5 children, join all: at most 3 slots are held at any moment, the
//    waiting parent holds none, 3 children run at once (so the parent's slot went to a child),
//    sandboxes never outnumber the slots plus the one waiting parent, and all 5 finish. \`seed\`
//    picks the order the children end in and the ticks between steps; `None` ends them in order.
async fn scenario_2(seed: Option<u64>) {
    let random = seed.is_some();
    let mut rng = Lcg(seed.unwrap_or(0));
    let mut h = harness(3);
    let (lead, _s) = parent(&mut h, false).await;
    let mut sessions: Vec<SessionCtl> = (0..5).map(|_| h.driver.live()).collect();
    let mut children = Vec::new();
    for _ in 0..5 {
        children.push(spawn(&h, lead).await);
        if random {
            settle_for(rng.next(3) as u32).await;
        }
    }
    let join = join_in_background(&h, lead, Vec::new(), JoinMode::All, None);
    let mut running_peak = 0;
    while !sessions.is_empty() {
        until("a child running", || {
            assert!(h.engine.slots_in_use() <= 3, "over the cap");
            assert!(
                h.driver.leaked().len() <= 4,
                "more sandboxes than slots plus the parent"
            );
            let running = children
                .iter()
                .filter(|c| h.is(**c, &RunStatus::Running))
                .count();
            running_peak = running_peak.max(running);
            running > 0
        })
        .await;
        if random {
            settle_for(rng.next(4) as u32).await;
        }
        assert!(
            !h.engine.holds_slot(lead),
            "the waiting parent holds no slot"
        );
        // End any one of the sessions still open (in order without a seed): one that is running,
        // or one queued for a child that has not started (that child then ends as it starts).
        let pick = if random { rng.next(sessions.len()) } else { 0 };
        sessions.remove(pick).exit(0);
    }
    let results = join.await.unwrap().unwrap();
    assert_eq!(results.len(), 5);
    for c in &children {
        assert!(matches!(
            outcome(&results, *c),
            JoinOutcome::Completed { .. }
        ));
    }
    // In order, three children run at once: the parent's slot went to one of them. A random order
    // can end a child before the others start, so the peak is only checked in order.
    if !random {
        assert_eq!(
            running_peak, 3,
            "the children used the slot the parent gave up"
        );
    }
    assert!(h.engine.holds_slot(lead));
}

#[tokio::test(start_paused = true)]
async fn s2_s3_fan_out_over_the_cap_finishes_and_never_overshoots() {
    scenario_2(None).await;
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
    for seed in 0..200 {
        scenario_2(Some(seed)).await;
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
    // The dropped join still gives the parent its slot back once one is free: it keeps running.
    until("parent has its slot", || h.engine.holds_slot(lead)).await;
    assert_eq!(h.engine.slots_in_use(), 1);
}

// A join dropped while it waits on its children (the caller went away) gives the parent its slot
// back once one is free, instead of leaving a running parent without one.
#[tokio::test(start_paused = true)]
async fn a_join_dropped_while_waiting_on_children_gives_the_slot_back_later() {
    let mut h = harness(1);
    let (lead, _s) = parent(&mut h, false).await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead).await;
    let join = join_in_background(&h, lead, vec![child], JoinMode::All, None);
    until("child running", || h.is(child, &RunStatus::Running)).await;
    assert!(!h.engine.holds_slot(lead));
    join.abort();
    settle().await;
    child_session.exit(0);
    until("parent has its slot", || h.engine.holds_slot(lead)).await;
    // Nothing else can start while the parent holds the only slot.
    let _other_session = h.driver.live();
    let other = h.submit(spec()).await;
    settle().await;
    assert!(h.is(other, &RunStatus::Queued));
}

// 6, with every slot taken by the children: the timeout still bounds the join. The parent runs
// over the cap until a slot frees, then has it again.
#[tokio::test(start_paused = true)]
async fn s6_a_timeout_bounds_the_join_even_when_the_cap_is_full() {
    let mut h = harness(1);
    let (lead, _s) = parent(&mut h, false).await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead).await;
    // The parent holds the one slot, so the child starts only once the join gives it back.
    let started = tokio::time::Instant::now();
    let join = join_in_background(
        &h,
        lead,
        vec![child],
        JoinMode::All,
        Some(Duration::from_secs(5)),
    );
    until("child running", || h.is(child, &RunStatus::Running)).await;
    let results = join.await.unwrap().unwrap();
    assert!(
        started.elapsed() <= Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(outcome(&results, child), JoinOutcome::TimedOut);
    assert!(
        !h.engine.holds_slot(lead),
        "the child still has the only slot"
    );
    child_session.exit(0);
    until("parent has its slot", || h.engine.holds_slot(lead)).await;
}

// Two joins by one parent at once: the slot is given back by the first and taken back by the
// last, so a parent never holds a slot while one of its joins still waits.
#[tokio::test(start_paused = true)]
async fn overlapping_joins_hold_no_slot_until_the_last_one_ends() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    let a_session = h.driver.live();
    let a = spawn(&h, lead).await;
    let b_session = h.driver.live();
    let b = spawn(&h, lead).await;
    until("both running", || {
        h.is(a, &RunStatus::Running) && h.is(b, &RunStatus::Running)
    })
    .await;
    let join_a = join_in_background(&h, lead, vec![a], JoinMode::All, None);
    let join_b = join_in_background(&h, lead, vec![b], JoinMode::All, None);
    settle().await;
    assert!(!h.engine.holds_slot(lead));
    a_session.exit(0);
    join_a.await.unwrap().unwrap();
    settle().await;
    assert!(!join_b.is_finished());
    assert!(!h.engine.holds_slot(lead), "a join still waits");
    b_session.exit(0);
    join_b.await.unwrap().unwrap();
    assert!(h.engine.holds_slot(lead));
}

// A parent that starts waiting on an approval while a join waits (a parallel tool call) keeps
// its sandbox, so it gets its slot back like a running one.
#[tokio::test(start_paused = true)]
async fn a_parent_waiting_on_an_approval_gets_its_slot_back() {
    let mut h = harness(2);
    let (lead, session) = parent(&mut h, false).await;
    let child_session = h.driver.live();
    let child = spawn(&h, lead).await;
    until("child running", || h.is(child, &RunStatus::Running)).await;
    let join = join_in_background(&h, lead, vec![child], JoinMode::All, None);
    settle().await;
    let aid = kestrel_types::ApprovalId::new();
    session.line(&format!("approval:{aid}:deploy"));
    until("parent waits on approval", || {
        h.is(
            lead,
            &RunStatus::Suspended(kestrel_types::SuspendReason::Approval(aid)),
        )
    })
    .await;
    child_session.exit(0);
    join.await.unwrap().unwrap();
    assert!(h.engine.holds_slot(lead));
}

// 3. Cancel reaches runs below a child that ended and kept its children.
#[tokio::test(start_paused = true)]
async fn cancel_reaches_runs_below_a_finished_child_that_kept_its_children() {
    let mut h = harness(8);
    let (root, _s) = parent(&mut h, true).await;
    let mid_session = h.driver.live();
    let mid = h
        .engine
        .spawn_child(root, &agent("lead", true), "mid", Some(MicroUsd(DOLLAR)))
        .await
        .unwrap();
    until("mid running", || h.is(mid, &RunStatus::Running)).await;
    let _g = h.driver.live();
    let grand = spawn(&h, mid).await;
    until("grandchild running", || h.is(grand, &RunStatus::Running)).await;
    mid_session.exit(0);
    until("mid completed", || h.is(mid, &RunStatus::Completed)).await;
    assert!(h.is(grand, &RunStatus::Running), "mid kept its child");
    h.control(root, ControlCommand::Cancel);
    until("all ended", || {
        h.is(root, &RunStatus::Cancelled) && h.is(grand, &RunStatus::Cancelled)
    })
    .await;
    until("sandboxes destroyed", || h.driver.leaked().is_empty()).await;
}

// A join that times out leaves its slot to come back later. A second join started before that
// happens must not let the parent take the only slot its new child needs.
#[tokio::test(start_paused = true)]
async fn a_join_after_a_timed_out_join_does_not_deadlock() {
    let mut h = harness(1);
    let (lead, _s) = parent(&mut h, false).await;
    let first_session = h.driver.live();
    let first = spawn(&h, lead).await;
    let timed = h
        .engine
        .join(lead, &[first], JoinMode::All, Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(outcome(&timed, first), JoinOutcome::TimedOut);
    // The timed-out join's way back is in line before the second child is spawned.
    settle().await;
    let second_session = h.driver.live();
    let second = spawn(&h, lead).await;
    let join = join_in_background(&h, lead, vec![first, second], JoinMode::All, None);
    settle().await;
    first_session.exit(0);
    until("second running", || h.is(second, &RunStatus::Running)).await;
    assert!(
        !h.engine.holds_slot(lead),
        "the waiting parent holds no slot"
    );
    second_session.exit(0);
    let results = join.await.unwrap().unwrap();
    assert_eq!(results.len(), 2);
    assert!(h.engine.holds_slot(lead));
}

// A join that starts while another of the same parent is still taking the slot back: the parent
// must not hold the slot while the new join waits.
#[tokio::test(start_paused = true)]
async fn a_join_that_starts_while_the_slot_comes_back_holds_no_slot() {
    let mut h = harness(2);
    let (lead, _s) = parent(&mut h, false).await;
    let a_session = h.driver.live();
    let a = spawn(&h, lead).await;
    until("a running", || h.is(a, &RunStatus::Running)).await;
    let join_a = join_in_background(&h, lead, vec![a], JoinMode::All, None);
    settle().await;
    // A queued run takes the slot the parent gave up, then one more waits in line, so when `a`
    // ends its slot goes to that run and join A waits behind it for the parent's slot.
    let other_session = h.driver.live();
    let other = h.submit(spec()).await;
    until("other running", || h.is(other, &RunStatus::Running)).await;
    let queued_session = h.driver.live();
    let queued = h.submit(spec()).await;
    settle().await;
    a_session.exit(0);
    until("queued running", || h.is(queued, &RunStatus::Running)).await;
    // Join B starts while join A still waits for the slot.
    let b_session = h.driver.live();
    let b = spawn(&h, lead).await;
    let join_b = join_in_background(&h, lead, vec![b], JoinMode::All, None);
    settle().await;
    other_session.exit(0);
    until("b running", || h.is(b, &RunStatus::Running)).await;
    assert!(!h.engine.holds_slot(lead), "join B still waits");
    join_a.await.unwrap().unwrap();
    assert!(!join_b.is_finished());
    queued_session.exit(0);
    b_session.exit(0);
    join_b.await.unwrap().unwrap();
    until("parent has its slot", || h.engine.holds_slot(lead)).await;
}

// A child may keep its children only when its parent keeps its own.
#[tokio::test(start_paused = true)]
async fn keep_children_is_narrowed_by_the_parent() {
    let mut h = harness(4);
    let (lead, _s) = parent(&mut h, false).await;
    let child = h
        .engine
        .spawn_child(lead, &agent("lead", true), "x", Some(MicroUsd(DOLLAR / 10)))
        .await
        .unwrap();
    let keep = h
        .store
        .run_spec(child)
        .unwrap()
        .unwrap()
        .capabilities
        .spawn
        .is_some_and(|s| s.keep_children);
    assert!(!keep);
}

// 13, with a child still running at the restart: it comes back suspended, the engine resumes it by
// itself (#92), and once it ends, a new join returns its result.
#[tokio::test(start_paused = true)]
async fn s13_a_child_running_at_the_restart_is_joined_after_it_is_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let mut h = Harness::on(&db, 4);
    h.rebuild(|e| e.with_root_key(RootKey::from_bytes(&key).unwrap()));
    let (lead, _s) = parent(&mut h, false).await;
    let _c = h.driver.live();
    let child = spawn(&h, lead).await;
    until("child running", || h.is(child, &RunStatus::Running)).await;
    h.shutdown.cancel();
    if let Some(task) = h.task.take() {
        task.await.unwrap().unwrap();
    }
    drop(h);

    // One slot: the lead, resumed first, holds it until it joins; then the child runs.
    let mut again = Harness::on(&db, 1);
    again.rebuild(|e| e.with_root_key(RootKey::from_bytes(&key).unwrap()));
    again.engine.recover().await.unwrap();
    let _lead = again.driver.live();
    again.driver.script(&["ok:after the restart"], 0);
    again.start();
    until("the lead runs again", || {
        again.is(lead, &RunStatus::Running)
    })
    .await;
    let results = again
        .engine
        .join(lead, &[child], JoinMode::All, Some(Duration::from_secs(60)))
        .await
        .unwrap();
    assert_eq!(
        outcome(&results, child),
        JoinOutcome::Completed {
            summary: Some("after the restart".into())
        }
    );
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
