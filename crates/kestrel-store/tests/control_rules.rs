//! Issue #90: which control command a run's status allows. The CLI and the API refuse the rest up
//! front with the reason this rule gives; the engine applies the same rule.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_store::ControlCommand;
use kestrel_types::{ApprovalId, MicroUsd, RunStatus, SuspendReason};

fn states(aid: ApprovalId) -> Vec<(&'static str, RunStatus)> {
    vec![
        ("Queued", RunStatus::Queued),
        ("Provisioning", RunStatus::Provisioning),
        ("Running", RunStatus::Running),
        (
            "Susp(Approval)",
            RunStatus::Suspended(SuspendReason::Approval(aid)),
        ),
        ("Susp(Budget)", RunStatus::Suspended(SuspendReason::Budget)),
        (
            "Susp(Error)",
            RunStatus::Suspended(SuspendReason::Error("engine restarted".into())),
        ),
        ("Susp(Manual)", RunStatus::Suspended(SuspendReason::Manual)),
        ("Completed", RunStatus::Completed),
        ("Failed", RunStatus::Failed("boom".into())),
        ("Cancelled", RunStatus::Cancelled),
    ]
}

const T: bool = true;
const F: bool = false;

// Rows = commands below, columns = `states()` in order. True where the engine applies the command.
#[rustfmt::skip]
const TABLE: [[bool; 10]; 8] = [
    //                    Q  P  R  SA SB SE SM Co Fa Ca
    /* Pause          */ [F, F, T, F, F, F, F, F, F, F],
    /* Resume         */ [F, F, F, F, T, T, T, F, F, F],
    /* Cancel         */ [T, T, T, T, T, T, T, F, F, F],
    /* SetBudget      */ [T, T, T, T, T, T, T, F, F, F],
    /* Approve(aid)   */ [F, F, F, T, F, F, F, F, F, F],
    /* Reject(aid)    */ [F, F, F, T, F, F, F, F, F, F],
    /* Approve(other) */ [F, F, F, F, F, F, F, F, F, F],
    /* Ask            */ [F, F, T, F, F, F, F, F, F, F],
];

fn commands(aid: ApprovalId) -> Vec<(&'static str, ControlCommand)> {
    vec![
        ("Pause", ControlCommand::Pause),
        ("Resume", ControlCommand::Resume),
        ("Cancel", ControlCommand::Cancel),
        ("SetBudget", ControlCommand::SetBudget(MicroUsd(3_000_000))),
        ("Approve(aid)", ControlCommand::Approve(aid)),
        (
            "Reject(aid)",
            ControlCommand::RejectWith {
                id: aid,
                reason: "no".into(),
            },
        ),
        ("Approve(other)", ControlCommand::Approve(ApprovalId::new())),
        (
            "Ask",
            ControlCommand::Ask {
                id: ApprovalId::new(),
                question: "which branch?".into(),
            },
        ),
    ]
}

#[test]
fn the_rule_matches_the_table_for_every_command_and_status() {
    let aid = ApprovalId::new();
    for (i, (cmd_name, cmd)) in commands(aid).iter().enumerate() {
        for (j, (status_name, status)) in states(aid).iter().enumerate() {
            let refusal = cmd.refusal(status);
            assert_eq!(
                refusal.is_none(),
                TABLE[i][j],
                "{cmd_name} on {status_name}: {refusal:?}"
            );
        }
    }
}

#[test]
fn an_ended_run_refuses_every_command_and_says_it_has_ended() {
    let aid = ApprovalId::new();
    for (status, name) in [
        (RunStatus::Completed, "completed"),
        (RunStatus::Failed("boom".into()), "failed"),
        (RunStatus::Cancelled, "cancelled"),
    ] {
        for (cmd_name, cmd) in commands(aid) {
            let why = cmd.refusal(&status).expect("refused");
            assert!(
                why.contains(&format!("the run has ended ({name})")),
                "{cmd_name} on {name}: {why}"
            );
        }
    }
}

#[test]
fn a_queued_run_cannot_be_paused_or_resumed_and_says_so() {
    let why = ControlCommand::Pause
        .refusal(&RunStatus::Queued)
        .expect("refused");
    assert!(why.contains("a queued run cannot be paused"), "{why}");
    let why = ControlCommand::Resume
        .refusal(&RunStatus::Queued)
        .expect("refused");
    assert!(why.contains("a queued run cannot be resumed"), "{why}");
    let why = ControlCommand::Pause
        .refusal(&RunStatus::Suspended(SuspendReason::Manual))
        .expect("refused");
    assert!(why.contains("a suspended run cannot be paused"), "{why}");
    let why = ControlCommand::Resume
        .refusal(&RunStatus::Running)
        .expect("refused");
    assert!(why.contains("a running run cannot be resumed"), "{why}");
}

#[test]
fn a_run_waiting_on_an_approval_resumes_through_its_decision() {
    let aid = ApprovalId::new();
    let why = ControlCommand::Resume
        .refusal(&RunStatus::Suspended(SuspendReason::Approval(aid)))
        .expect("refused");
    assert!(why.contains(&aid.to_string()), "{why}");
    assert!(why.contains("approve or reject"), "{why}");
}

#[test]
fn an_approval_the_run_does_not_wait_on_is_named() {
    let other = ApprovalId::new();
    let why = ControlCommand::Approve(other)
        .refusal(&RunStatus::Suspended(SuspendReason::Approval(
            ApprovalId::new(),
        )))
        .expect("refused");
    assert!(why.contains(&other.to_string()), "{why}");
    let why = ControlCommand::Ask {
        id: other,
        question: "q".into(),
    }
    .refusal(&RunStatus::Queued)
    .expect("refused");
    assert!(why.contains("a queued run"), "{why}");
}
