//! Scenarios 7 and 8: steering commands write the right control command, checked through
//! `Store::take_control`, and a negative budget is rejected with exit 2. Issue #90: a command the
//! run's status does not allow is refused with exit 1 and never written.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Fixture, agent, json_doc, running_run, walk};
use kestrel_store::{ControlCommand, Store};
use kestrel_types::{ApprovalId, MicroUsd, RunId, RunStatus, SuspendReason};
use predicates::prelude::*;

fn commands(store: &Store) -> Vec<(RunId, ControlCommand)> {
    store
        .take_control()
        .expect("take control")
        .into_iter()
        .map(|(_, run, cmd)| (run, cmd))
        .collect()
}

/// A run suspended on approval `aid`.
fn awaiting_approval(store: &Store) -> (RunId, ApprovalId) {
    let id = running_run(store);
    let aid = ApprovalId::new();
    walk(
        store,
        id,
        RunStatus::Running,
        &[RunStatus::Suspended(SuspendReason::Approval(aid))],
    );
    (id, aid)
}

/// Pauses running run `id` the way the engine does.
fn suspend(store: &Store, id: RunId) {
    walk(
        store,
        id,
        RunStatus::Running,
        &[RunStatus::Suspended(SuspendReason::Manual)],
    );
}

#[test]
fn s7_pause_resume_cancel_write_their_commands() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_run(&store);
    f.kestrel()
        .args(["pause", &id.to_string()])
        .assert()
        .success();
    assert_eq!(commands(&store), vec![(id, ControlCommand::Pause)]);
    // Resume needs a suspended run (#90).
    suspend(&store, id);
    for (cmd, want) in [
        ("resume", ControlCommand::Resume),
        ("cancel", ControlCommand::Cancel),
    ] {
        f.kestrel().args([cmd, &id.to_string()]).assert().success();
        assert_eq!(commands(&store), vec![(id, want)], "after `{cmd}`");
    }
}

#[test]
fn s7_approve_writes_approve_for_the_owning_run() {
    let f = Fixture::new();
    let store = f.store();
    let (id, aid) = awaiting_approval(&store);
    f.kestrel()
        .args(["approve", &aid.to_string()])
        .assert()
        .success();
    assert_eq!(commands(&store), vec![(id, ControlCommand::Approve(aid))]);
}

#[test]
fn s7_approve_reject_writes_reject() {
    let f = Fixture::new();
    let store = f.store();
    let (id, aid) = awaiting_approval(&store);
    f.kestrel()
        .args(["approve", &aid.to_string(), "--reject"])
        .assert()
        .success();
    assert_eq!(commands(&store), vec![(id, ControlCommand::Reject(aid))]);
}

#[test]
fn s7_budget_writes_set_budget_in_micro_usd() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_run(&store);
    f.kestrel()
        .args(["budget", &id.to_string(), "5"])
        .assert()
        .success();
    assert_eq!(
        commands(&store),
        vec![(id, ControlCommand::SetBudget(MicroUsd(5_000_000)))]
    );
    f.kestrel()
        .args(["budget", &id.to_string(), "2.50"])
        .assert()
        .success();
    assert_eq!(
        commands(&store),
        vec![(id, ControlCommand::SetBudget(MicroUsd(2_500_000)))]
    );
}

#[test]
fn s7_steering_an_unknown_run_exits_1() {
    let f = Fixture::new();
    let unknown = RunId::new().to_string();
    for args in [
        vec!["pause", unknown.as_str()],
        vec!["resume", unknown.as_str()],
        vec!["cancel", unknown.as_str()],
        vec!["budget", unknown.as_str(), "5"],
    ] {
        f.kestrel()
            .args(&args)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("no such run"));
    }
}

#[test]
fn s7_approving_an_unknown_approval_exits_1() {
    let f = Fixture::new();
    let store = f.store();
    let _ = running_run(&store);
    f.kestrel()
        .args(["approve", &ApprovalId::new().to_string()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("approval"));
    assert!(commands(&store).is_empty());
}

#[test]
fn s7_steering_commands_support_json() {
    let f = Fixture::new();
    let store = f.store();
    let (waiting, aid) = awaiting_approval(&store);
    let id = running_run(&store);
    let run = id.to_string();
    let approval = aid.to_string();
    let json_ok = |args: &[&str], owner: RunId| {
        let out = f.kestrel().args(args).assert().success();
        let doc = json_doc(&out.get_output().stdout);
        assert_eq!(doc["run_id"], owner.to_string(), "{args:?}: {doc}");
    };
    json_ok(&["pause", &run, "--json"], id);
    json_ok(&["budget", &run, "3", "--json"], id);
    suspend(&store, id);
    json_ok(&["resume", &run, "--json"], id);
    json_ok(&["approve", &approval, "--json"], waiting);
    json_ok(&["cancel", &run, "--json"], id);
    assert_eq!(commands(&store).len(), 5);
}

#[test]
fn s7_malformed_ids_exit_2() {
    let f = Fixture::new();
    for args in [
        vec!["pause", "nope"],
        vec!["approve", "nope"],
        vec!["budget", "nope", "5"],
    ] {
        f.kestrel().args(&args).assert().code(2);
    }
}

#[test]
fn s8_negative_budget_exits_2_and_writes_nothing() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_run(&store);
    f.kestrel()
        .args(["budget", &id.to_string(), "-1"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty().not());
    assert!(commands(&store).is_empty());
}

#[test]
fn s8_unparseable_budgets_exit_2() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_run(&store);
    // Not a number, and finer than one micro-dollar (never rounded).
    for usd in ["abc", "0.0000001", "", "1e3"] {
        f.kestrel()
            .args(["budget", &id.to_string(), usd])
            .assert()
            .code(2);
    }
    assert!(commands(&store).is_empty());
}

// Issue #90, scenario 1: a queued run cannot be paused (or resumed); cancel and budget still work.
#[test]
fn i90_s1_pausing_a_queued_run_exits_1_and_cancel_still_works() {
    let f = Fixture::new();
    let store = f.store();
    let id = store.create_run(&agent(), "task").expect("create run");
    let run = id.to_string();
    f.kestrel()
        .args(["pause", &run])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("a queued run cannot be paused"));
    f.kestrel()
        .args(["resume", &run])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("a queued run cannot be resumed"));
    assert!(commands(&store).is_empty(), "nothing reaches the engine");

    f.kestrel().args(["budget", &run, "3"]).assert().success();
    f.kestrel().args(["cancel", &run]).assert().success();
    assert_eq!(
        commands(&store),
        vec![
            (id, ControlCommand::SetBudget(MicroUsd(3_000_000))),
            (id, ControlCommand::Cancel)
        ]
    );
}

// Issue #90, scenario 2: an ended run refuses every steering command and nothing is written.
#[test]
fn i90_s2_an_ended_run_refuses_every_steering_command() {
    let f = Fixture::new();
    let store = f.store();
    let completed = running_run(&store);
    walk(
        &store,
        completed,
        RunStatus::Running,
        &[RunStatus::Completed],
    );
    let failed = running_run(&store);
    walk(
        &store,
        failed,
        RunStatus::Running,
        &[RunStatus::Failed("boom".into())],
    );
    let cancelled = store.create_run(&agent(), "task").expect("create run");
    walk(
        &store,
        cancelled,
        RunStatus::Queued,
        &[RunStatus::Cancelled],
    );
    for (id, name) in [
        (completed, "completed"),
        (failed, "failed"),
        (cancelled, "cancelled"),
    ] {
        let run = id.to_string();
        for args in [
            vec!["pause", run.as_str()],
            vec!["resume", run.as_str()],
            vec!["cancel", run.as_str()],
            vec!["budget", run.as_str(), "3"],
            vec!["cancel", run.as_str(), "--json"],
        ] {
            f.kestrel()
                .args(&args)
                .assert()
                .code(1)
                .stdout(predicate::str::is_empty())
                .stderr(predicate::str::contains(format!(
                    "the run has ended ({name})"
                )))
                .stderr(predicate::str::contains(run.as_str()));
        }
    }
    assert!(commands(&store).is_empty(), "nothing reaches the engine");
}

// Issue #90: a run waiting on an approval resumes through its decision, not through `resume`;
// cancel and budget still reach it (scenario 5).
#[test]
fn i90_a_run_waiting_on_an_approval_is_not_resumed_but_can_be_cancelled() {
    let f = Fixture::new();
    let store = f.store();
    let (id, aid) = awaiting_approval(&store);
    let run = id.to_string();
    f.kestrel()
        .args(["resume", &run])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("approve or reject"))
        .stderr(predicate::str::contains(aid.to_string()));
    f.kestrel()
        .args(["pause", &run])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("a suspended run cannot be paused"));
    assert!(commands(&store).is_empty());
    f.kestrel().args(["budget", &run, "1"]).assert().success();
    f.kestrel()
        .args(["cancel", &run])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("sent cancel to {run}")));
    assert_eq!(
        commands(&store),
        vec![
            (id, ControlCommand::SetBudget(MicroUsd(1_000_000))),
            (id, ControlCommand::Cancel)
        ]
    );
}

// PR #108 review: a root that ended and kept its children (`spawn.keep_children`) is still the
// handle on its live tree. Cancel is accepted (the engine cancels every live run below it); every
// other command is still refused, and so is cancel once nothing below is live.
#[test]
fn i90_cancel_on_an_ended_root_with_a_live_kept_child_is_sent() {
    let f = Fixture::new();
    let store = f.store();
    let root = running_run(&store);
    let kid = store
        .create_child_run(root, &agent(), "kid")
        .expect("create child");
    walk(
        &store,
        kid,
        RunStatus::Queued,
        &[RunStatus::Provisioning, RunStatus::Running],
    );
    walk(&store, root, RunStatus::Running, &[RunStatus::Completed]);
    let run = root.to_string();

    f.kestrel()
        .args(["cancel", &run])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("sent cancel to {run}")));
    assert_eq!(commands(&store), vec![(root, ControlCommand::Cancel)]);
    for args in [
        vec!["pause", run.as_str()],
        vec!["budget", run.as_str(), "3"],
    ] {
        f.kestrel()
            .args(&args)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("the run has ended (completed)"));
    }

    walk(&store, kid, RunStatus::Running, &[RunStatus::Cancelled]);
    f.kestrel()
        .args(["cancel", &run])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("the run has ended (completed)"));
    assert!(commands(&store).is_empty());
}
