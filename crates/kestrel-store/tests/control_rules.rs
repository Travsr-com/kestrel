//! Issue #90: which control command a run's status allows. The CLI and the API refuse the rest up
//! front with the reason this rule gives; the engine applies the same rule.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_store::{ControlCommand, Store};
use kestrel_types::{AgentSpec, ApprovalId, MicroUsd, RunId, RunStatus, SuspendReason};

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
    // Engine-internal (the gateway's `kestrel.ask_human`): the engine answers it in any status,
    // so the rule never refuses it (PR #108 review).
    /* Ask            */ [T, T, T, T, T, T, T, T, T, T],
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
        for (cmd_name, cmd) in commands(aid).into_iter().filter(|(n, _)| *n != "Ask") {
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
}

/// A run in `status`, created and moved there through the store.
fn run_in(store: &Store, parent: Option<RunId>, status: &RunStatus) -> RunId {
    let id = match parent {
        Some(p) => store.create_child_run(p, &agent(), "x").unwrap(),
        None => store.create_run(&agent(), "x").unwrap(),
    };
    let path: &[RunStatus] = match status {
        RunStatus::Queued => &[],
        RunStatus::Cancelled => &[RunStatus::Cancelled],
        RunStatus::Running => &[RunStatus::Provisioning, RunStatus::Running],
        _ => &[RunStatus::Provisioning, RunStatus::Running, status.clone()],
    };
    let mut from = RunStatus::Queued;
    for next in path {
        store.transition(id, &from, next.clone()).unwrap();
        from = next.clone();
    }
    id
}

fn agent() -> AgentSpec {
    AgentSpec::from_yaml(
        "name: a\nruntime: opencode\nrole: r\n\
         sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }\n\
         budget: { usd: 1.00, wall_clock: 10m }\n",
    )
    .unwrap()
}

// PR #108 review: the live runs below a run are its nearest ones that have not ended, found
// through children that ended (and kept theirs). The engine and the up-front check both use it.
#[test]
fn live_below_finds_the_nearest_live_runs_through_ended_children() {
    let store = Store::open_in_memory().unwrap();
    let root = run_in(&store, None, &RunStatus::Completed);
    let live = run_in(&store, Some(root), &RunStatus::Running);
    let _under_live = run_in(&store, Some(live), &RunStatus::Queued);
    let ended = run_in(&store, Some(root), &RunStatus::Completed);
    let kept = run_in(
        &store,
        Some(ended),
        &RunStatus::Suspended(SuspendReason::Manual),
    );
    let gone = run_in(&store, Some(root), &RunStatus::Cancelled);
    let _ = run_in(&store, Some(gone), &RunStatus::Failed("boom".into()));

    let mut below = store.live_below(root).unwrap();
    below.sort();
    let mut want = vec![live, kept];
    want.sort();
    assert_eq!(below, want);
    assert_eq!(store.live_below(gone).unwrap(), Vec::<RunId>::new());
    assert_eq!(store.live_below(RunId::new()).unwrap(), Vec::<RunId>::new());
}

// PR #108 review: cancel on an ended run that still has a live run below it cancels that tree, as
// the engine does, so it is not refused. Every other command, and cancel on an ended run with
// nothing live below it, still is.
#[test]
fn cancel_is_allowed_on_an_ended_run_with_a_live_run_below_it() {
    let store = Store::open_in_memory().unwrap();
    let root = run_in(&store, None, &RunStatus::Completed);
    let kid = run_in(&store, Some(root), &RunStatus::Running);
    let row = store.get_run(root).unwrap().unwrap();
    assert_eq!(
        store
            .command_refusal(&row, &ControlCommand::Cancel)
            .unwrap(),
        None
    );
    for cmd in [
        ControlCommand::Pause,
        ControlCommand::Resume,
        ControlCommand::SetBudget(MicroUsd(1)),
    ] {
        let why = store.command_refusal(&row, &cmd).unwrap().expect("refused");
        assert!(
            why.contains("the run has ended (completed)"),
            "{cmd:?}: {why}"
        );
    }

    store
        .transition(kid, &RunStatus::Running, RunStatus::Completed)
        .unwrap();
    let why = store
        .command_refusal(&row, &ControlCommand::Cancel)
        .unwrap()
        .expect("refused once nothing below is live");
    assert!(why.contains("the run has ended (completed)"), "{why}");

    // A live run: the status rule alone decides, nothing below is read.
    let live = store
        .get_run(run_in(&store, None, &RunStatus::Queued))
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .command_refusal(&live, &ControlCommand::Cancel)
            .unwrap(),
        None
    );
    assert!(
        store
            .command_refusal(&live, &ControlCommand::Pause)
            .unwrap()
            .is_some()
    );
}
