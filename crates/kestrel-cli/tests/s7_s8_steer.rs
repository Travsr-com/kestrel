//! Scenarios 7 and 8: steering commands write the right control command, checked through
//! `Store::take_control`, and a negative budget is rejected with exit 2.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Fixture, json_doc, running_run, walk};
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

#[test]
fn s7_pause_resume_cancel_write_their_commands() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_run(&store);
    for (cmd, want) in [
        ("pause", ControlCommand::Pause),
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
    let (id, aid) = awaiting_approval(&store);
    let run = id.to_string();
    let approval = aid.to_string();
    for args in [
        vec!["pause", run.as_str(), "--json"],
        vec!["resume", run.as_str(), "--json"],
        vec!["budget", run.as_str(), "3", "--json"],
        vec!["approve", approval.as_str(), "--json"],
        vec!["cancel", run.as_str(), "--json"],
    ] {
        let out = f.kestrel().args(&args).assert().success();
        let doc = json_doc(&out.get_output().stdout);
        assert_eq!(doc["run_id"], run.as_str(), "{args:?}: {doc}");
    }
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
