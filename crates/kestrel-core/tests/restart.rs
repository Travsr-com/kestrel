//! Issue #92: a tree carries on after `kestrel serve` is stopped and started again.
//!
//! * Recovery suspends what the restart cut off (`Running` and `Provisioning` runs) with
//!   `Suspended(Error("engine restarted"))` and resumes those runs by itself once the engine runs
//!   (`Engine::with_resume_after_restart`, on by default). Manual, budget and approval suspensions
//!   stay as they are.
//! * A resumed agent starts again from its input, so it spawns its children again. A spawn that
//!   matches one the parent made before (same agent, same input, the same count of such spawns so
//!   far in this start of its agent) returns that child, live or finished, and does not count
//!   against `max_children` again.
//! * A join from the re-run parent then returns each child's result once.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use kestrel_core::caps::RootKey;
use kestrel_core::{ChildResult, JoinMode, JoinOutcome, SpawnedChild};
use kestrel_store::ControlCommand;
use kestrel_types::{AgentSpec, ApprovalId, EventKind, MicroUsd, RunId, RunStatus, SuspendReason};

const DOLLAR: i64 = 1_000_000;
const RESTARTED: &str = "engine restarted";

fn restarted() -> RunStatus {
    RunStatus::Suspended(SuspendReason::Error(RESTARTED.into()))
}

/// A lead that may spawn `max_children` workers.
fn lead_spec(max_children: u32) -> AgentSpec {
    let yaml = format!(
        "name: lead\nruntime: claude-code\nrole: r\n\
         capabilities: {{ mcp: [kestrel.spawn], spawn: {{ max_children: {max_children}, max_depth: 2, agents: [worker] }} }}\n\
         sandbox: {{ tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
         budget: {{ usd: \"10\", wall_clock: 1h }}\n"
    );
    AgentSpec::from_yaml(&yaml).unwrap()
}

fn worker() -> AgentSpec {
    let yaml = "name: worker\nruntime: claude-code\nrole: r\n\
         sandbox: { tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: empty-dir }\n\
         budget: { usd: \"10\", wall_clock: 1h }\n";
    AgentSpec::from_yaml(yaml).unwrap()
}

/// An engine with `slots` slots on `db`, signing with `key`.
fn harness_on(db: &Path, slots: usize, key: &[u8], resume: bool) -> Harness {
    let mut h = Harness::on(db, slots);
    h.rebuild(|e| {
        e.with_root_key(RootKey::from_bytes(key).unwrap())
            .with_resume_after_restart(resume)
    });
    h
}

async fn spawn_help(h: &Harness, parent: RunId) -> SpawnedChild {
    h.engine
        .spawn_child_with_report(parent, &worker(), "help", Some(MicroUsd(DOLLAR / 10)))
        .await
        .unwrap()
}

/// Stops the engine as a kill would: nothing changes in the store.
async fn stop(mut h: Harness) {
    h.shutdown.cancel();
    if let Some(task) = h.task.take() {
        task.await.unwrap().unwrap();
    }
}

fn completed(results: &[ChildResult], run: RunId) -> bool {
    results
        .iter()
        .filter(|r| r.run == run)
        .map(|r| &r.outcome)
        .eq([&JoinOutcome::Completed {
            summary: Some("done".into()),
        }])
}

fn provisioned(h: &Harness, run: RunId) -> usize {
    h.count_events(run, |k| matches!(k, EventKind::SandboxProvisioned { .. }))
}

fn ignored_commands(h: &Harness, run: RunId) -> usize {
    h.count_events(
        run,
        |k| matches!(k, EventKind::AgentLog { line } if line.contains("ignored")),
    )
}

/// A first engine with a running lead (max_children 3) and three running children it spawned with
/// the same agent and input, the lead waiting in its join. Then the engine stops.
async fn tree_in_a_join(db: &Path, key: &[u8]) -> (RunId, Vec<RunId>) {
    let mut h = harness_on(db, 4, key, true);
    let _lead = h.driver.live();
    h.start();
    let lead = h.submit(lead_spec(3)).await;
    until("lead running", || h.is(lead, &RunStatus::Running)).await;
    let _kids: Vec<SessionCtl> = (0..3).map(|_| h.driver.live()).collect();
    let mut kids = Vec::new();
    for _ in 0..3 {
        let spawned = spawn_help(&h, lead).await;
        assert!(!spawned.existing, "a first spawn is a new child");
        kids.push(spawned.id);
    }
    until("children running", || {
        kids.iter().all(|k| h.is(*k, &RunStatus::Running))
    })
    .await;
    let engine = h.engine.clone();
    let join = tokio::spawn(async move { engine.join(lead, &[], JoinMode::All, None).await });
    settle().await;
    assert!(!join.is_finished(), "the lead waits in its join");
    join.abort();
    stop(h).await;
    (lead, kids)
}

// Scenario 1: a parent in a join with 3 running children; serve stops and starts again. Without
// a manual resume the children carry on and the parent comes back; its spawns of the same children
// return them (none is spawned twice, none counts again: max_children is 3), and its join returns
// each result once.
#[tokio::test(start_paused = true)]
async fn s1_a_tree_in_a_join_carries_on_after_a_restart_without_a_resume() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let (lead, kids) = tree_in_a_join(&db, &key).await;

    // One slot: the lead (resumed first) holds it until it joins, then the children run in turn.
    let mut again = harness_on(&db, 1, &key, true);
    let _lead = again.driver.live();
    for _ in &kids {
        again.driver.script(&["ok:done"], 0);
    }
    let report = again.engine.recover().await.unwrap();
    let mut interrupted = report.interrupted.clone();
    interrupted.sort();
    let mut tree = kids.clone();
    tree.push(lead);
    tree.sort();
    assert_eq!(interrupted, tree);
    for run in &tree {
        assert!(again.is(*run, &restarted()), "{run} after recover");
    }
    again.start();
    until("the lead runs again by itself", || {
        again.is(lead, &RunStatus::Running)
    })
    .await;

    // Its agent starts again from its input and spawns the same three children.
    for kid in &kids {
        let spawned = spawn_help(&again, lead).await;
        assert_eq!(spawned.id, *kid);
        assert!(spawned.existing);
    }
    assert_eq!(again.store.children(lead).unwrap().len(), 3);
    assert!(again.has_event(lead, |k| matches!(
        k,
        EventKind::AgentLog { line } if line.contains(&kids[0].to_string())
    )));
    // A fourth spawn is a new one, past max_children.
    let refused = again
        .engine
        .spawn_child(lead, &worker(), "help", Some(MicroUsd(DOLLAR / 10)))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("max_children"), "{refused}");

    let results = again
        .engine
        .join(lead, &[], JoinMode::All, Some(Duration::from_secs(60)))
        .await
        .unwrap();
    assert_eq!(results.len(), 3, "{results:?}");
    for kid in &kids {
        assert!(completed(&results, *kid), "{kid}: {results:?}");
        assert_eq!(
            provisioned(&again, *kid),
            2,
            "one sandbox before, one after"
        );
        assert_eq!(ignored_commands(&again, *kid), 0);
    }
    assert_eq!(again.store.children(lead).unwrap().len(), 3);
}

// Scenario 2: with automatic resume off, nothing resumes by itself; the parent resumed by hand
// does not spawn its children again.
#[tokio::test(start_paused = true)]
async fn s2_a_parent_resumed_by_hand_gets_its_children_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let (lead, kids) = tree_in_a_join(&db, &key).await;

    let mut again = harness_on(&db, 4, &key, false);
    let _lead = again.driver.live();
    again.engine.recover().await.unwrap();
    again.start();
    settle().await;
    for run in kids.iter().chain([&lead]) {
        assert!(again.is(*run, &restarted()), "{run} stays suspended");
    }
    assert_eq!(again.driver.starts(), 0);

    again.control(lead, ControlCommand::Resume);
    until("the lead runs again", || {
        again.is(lead, &RunStatus::Running)
    })
    .await;
    for kid in &kids {
        let spawned = spawn_help(&again, lead).await;
        assert_eq!((spawned.id, spawned.existing), (*kid, true));
    }
    assert_eq!(again.store.children(lead).unwrap().len(), 3);
    for kid in &kids {
        assert!(
            again.is(*kid, &restarted()),
            "a child returned again stays as it was"
        );
    }
}

// Recovery resumes the runs the restart stopped, and those only: a paused run, a run over its
// budget, a run waiting for an approval and a run suspended for another error stay suspended. A
// run an earlier start suspended for the restart, whose resume never arrived, is resumed too. A
// second recover before the engine runs does not resume anything twice.
#[tokio::test(start_paused = true)]
async fn recovery_resumes_only_what_the_restart_stopped() {
    use RunStatus::{Provisioning, Queued, Running, Suspended};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let aid = ApprovalId::new();
    let other_error = Suspended(SuspendReason::Error("model refused".into()));
    // `earlier` was suspended for a restart by an engine that did not resume it (before this
    // upgrade, or with the setting off): it is left for a person.
    let (running, earlier, manual, budget, approval, errored) = {
        let first = harness_on(&db, 4, &key, true);
        let st = &first.store;
        let mut runs = Vec::new();
        for _ in 0..6 {
            let id = first.submit(spec()).await;
            st.transition(id, &Queued, Provisioning).unwrap();
            st.transition(id, &Provisioning, Running).unwrap();
            runs.push(id);
        }
        st.transition(runs[1], &Running, restarted()).unwrap();
        st.transition(runs[2], &Running, Suspended(SuspendReason::Manual))
            .unwrap();
        st.transition(runs[3], &Running, Suspended(SuspendReason::Budget))
            .unwrap();
        st.append(
            runs[4],
            EventKind::ApprovalRequested {
                id: aid,
                action: "deploy".into(),
                question: false,
            },
        )
        .unwrap();
        st.transition(runs[4], &Running, Suspended(SuspendReason::Approval(aid)))
            .unwrap();
        st.transition(runs[5], &Running, other_error.clone())
            .unwrap();
        (runs[0], runs[1], runs[2], runs[3], runs[4], runs[5])
    };

    let mut again = harness_on(&db, 4, &key, true);
    let _sessions: Vec<SessionCtl> = (0..2).map(|_| again.driver.live()).collect();
    let report = again.engine.recover().await.unwrap();
    assert_eq!(report.interrupted, vec![running]);
    again.engine.recover().await.unwrap();
    again.start();
    until("resumed", || again.is(running, &Running)).await;
    settle().await;
    assert!(
        again.is(earlier, &restarted()),
        "a run an earlier start did not resume stays suspended"
    );
    assert!(again.is(manual, &Suspended(SuspendReason::Manual)));
    assert!(again.is(budget, &Suspended(SuspendReason::Budget)));
    assert!(again.is(approval, &Suspended(SuspendReason::Approval(aid))));
    assert!(again.is(errored, &other_error));
    assert_eq!(again.driver.starts(), 1);
    assert_eq!(ignored_commands(&again, running), 0, "resumed once");
}

/// A first engine with one running run, stopped. Returns the run.
async fn one_running(db: &Path, key: &[u8]) -> RunId {
    use RunStatus::{Provisioning, Queued, Running};
    let first = harness_on(db, 4, key, true);
    let id = first.submit(spec()).await;
    first.store.transition(id, &Queued, Provisioning).unwrap();
    first.store.transition(id, &Provisioning, Running).unwrap();
    id
}

// A start with resume off suspends a run and does not mark it; a later start with resume on
// leaves it suspended, for a person.
#[tokio::test(start_paused = true)]
async fn a_run_stopped_while_resume_was_off_stays_suspended_when_it_is_turned_on() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let run = one_running(&db, &key).await;

    let off = harness_on(&db, 4, &key, false);
    assert_eq!(off.engine.recover().await.unwrap().interrupted, vec![run]);
    drop(off);

    let mut on = harness_on(&db, 4, &key, true);
    let report = on.engine.recover().await.unwrap();
    assert_eq!(report.suspended, vec![run]);
    let _session = on.driver.live();
    on.start();
    settle().await;
    assert!(on.is(run, &restarted()), "left for a person");
    assert_eq!(on.driver.starts(), 0);
}

// A resuming start queued a resume, which was claimed and lost when serve stopped again before a
// slot freed. The next start queues it again: the run was stopped by a resuming start.
#[tokio::test(start_paused = true)]
async fn a_resume_lost_to_a_second_stop_is_queued_again() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let run = one_running(&db, &key).await;

    let first = harness_on(&db, 4, &key, true);
    first.engine.recover().await.unwrap();
    // A scheduler claimed the resume and died before delivering it.
    let claimed = first.store.take_control().unwrap();
    assert_eq!(claimed.len(), 1, "{claimed:?}");
    drop(first);

    let mut again = harness_on(&db, 4, &key, true);
    assert_eq!(again.engine.recover().await.unwrap().suspended, vec![run]);
    let _session = again.driver.live();
    again.start();
    until("resumed", || again.is(run, &RunStatus::Running)).await;
    assert_eq!(again.driver.starts(), 1);
}

/// A first engine with one running run that `restarts_before` earlier restarts already stopped
/// and resumed. Returns the run.
async fn restarted_before(db: &Path, key: &[u8], restarts_before: usize) -> RunId {
    let run = one_running(db, key).await;
    let st = kestrel_store::Store::open(db).unwrap();
    for _ in 0..restarts_before {
        st.transition(run, &RunStatus::Running, restarted())
            .unwrap();
        st.transition(run, &restarted(), RunStatus::Running)
            .unwrap();
    }
    run
}

fn left_for_a_person(h: &Harness, run: RunId) -> bool {
    h.has_event(
        run,
        |k| matches!(k, EventKind::AgentLog { line } if line.contains("not resumed automatically")),
    )
}

// A run that takes serve down is not resumed on every start: after three restarts stopped it,
// the fourth leaves it suspended and says why.
#[tokio::test(start_paused = true)]
async fn a_run_stopped_by_a_fourth_restart_is_left_for_a_person() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let run = restarted_before(&db, &key, 3).await;

    let mut h = harness_on(&db, 4, &key, true);
    assert_eq!(h.engine.recover().await.unwrap().interrupted, vec![run]);
    assert!(left_for_a_person(&h, run));
    let _session = h.driver.live();
    h.start();
    settle().await;
    assert!(h.is(run, &restarted()), "not resumed");
    assert_eq!(h.driver.starts(), 0);

    // A person can still resume it.
    h.control(run, ControlCommand::Resume);
    until("resumed by hand", || h.is(run, &RunStatus::Running)).await;
}

// Three restarts are still resumed: the third stop is resumed like the first.
#[tokio::test(start_paused = true)]
async fn a_run_stopped_by_a_third_restart_is_still_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let run = restarted_before(&db, &key, 2).await;

    let mut h = harness_on(&db, 4, &key, true);
    assert_eq!(h.engine.recover().await.unwrap().interrupted, vec![run]);
    assert!(!left_for_a_person(&h, run));
    let _session = h.driver.live();
    h.start();
    until("resumed", || h.is(run, &RunStatus::Running)).await;
}

// Scenario 3: a child still `Provisioning` at the restart (its sandbox never started) is suspended
// with the others, starts once after the restart, and the join counts it.
#[tokio::test(start_paused = true)]
async fn s3_a_child_provisioning_at_the_restart_starts_once_and_is_joined() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kestrel.db");
    let key = RootKey::generate().to_bytes();
    let (lead, child) = {
        let mut h = harness_on(&db, 4, &key, true);
        let _lead = h.driver.live();
        h.start();
        let lead = h.submit(lead_spec(3)).await;
        until("lead running", || h.is(lead, &RunStatus::Running)).await;
        let _gate = h.driver.gate_create();
        let child = spawn_help(&h, lead).await.id;
        until("child provisioning", || {
            h.is(child, &RunStatus::Provisioning)
        })
        .await;
        settle().await;
        assert_eq!(provisioned(&h, child), 0);
        stop(h).await;
        (lead, child)
    };

    let mut again = harness_on(&db, 1, &key, true);
    let _lead = again.driver.live();
    again.driver.script(&["ok:done"], 0);
    let report = again.engine.recover().await.unwrap();
    assert!(report.interrupted.contains(&child), "{report:?}");
    assert!(again.is(child, &restarted()));
    again.start();
    until("the lead runs again", || {
        again.is(lead, &RunStatus::Running)
    })
    .await;
    let spawned = spawn_help(&again, lead).await;
    assert_eq!((spawned.id, spawned.existing), (child, true));
    let results = again
        .engine
        .join(lead, &[], JoinMode::All, Some(Duration::from_secs(60)))
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(completed(&results, child), "{results:?}");
    assert_eq!(provisioned(&again, child), 1, "started once");
    assert_eq!(
        again.transitions(child).last(),
        Some(&(RunStatus::Running, RunStatus::Completed))
    );
    assert!(
        again
            .transitions(child)
            .contains(&(RunStatus::Provisioning, restarted()))
    );
}

// Within one start of its agent, the same spawn twice is two children. A parent whose agent starts
// again (paused and resumed, in the same engine) gets them back in order; one more is new.
#[tokio::test(start_paused = true)]
async fn a_parent_started_again_gets_its_children_back_in_order() {
    let mut h = Harness::new(4);
    let key = RootKey::generate().to_bytes();
    h.rebuild(|e| e.with_root_key(RootKey::from_bytes(&key).unwrap()));
    let _lead = h.driver.live();
    h.start();
    let lead = h.submit(lead_spec(5)).await;
    until("lead running", || h.is(lead, &RunStatus::Running)).await;
    let first = spawn_help(&h, lead).await;
    let second = spawn_help(&h, lead).await;
    assert_ne!(first.id, second.id);
    assert!(!first.existing && !second.existing);
    until("children running", || {
        h.is(first.id, &RunStatus::Running) && h.is(second.id, &RunStatus::Running)
    })
    .await;

    h.control(lead, ControlCommand::Pause);
    until("paused", || {
        h.is(lead, &RunStatus::Suspended(SuspendReason::Manual))
    })
    .await;
    let _again = h.driver.live();
    h.control(lead, ControlCommand::Resume);
    until("running again", || h.is(lead, &RunStatus::Running)).await;
    let a = spawn_help(&h, lead).await;
    let b = spawn_help(&h, lead).await;
    let c = spawn_help(&h, lead).await;
    assert_eq!((a.id, a.existing), (first.id, true));
    assert_eq!((b.id, b.existing), (second.id, true));
    assert!(!c.existing);
    assert_eq!(h.store.children(lead).unwrap().len(), 3);
}
