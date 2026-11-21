//! Issue #37: approvals as the API lists them, read from the event log, and the control commands
//! that resolve one with text.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_store::{ControlCommand, Store};
use kestrel_types::{AgentSpec, ApprovalId, EventKind, RunId};

fn agent() -> AgentSpec {
    AgentSpec::from_yaml(
        "name: a\nruntime: opencode\nrole: r\n\
         sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }\n\
         budget: { usd: 1.00, wall_clock: 10m }\n",
    )
    .unwrap()
}

fn request(store: &Store, run: RunId, action: &str) -> ApprovalId {
    let id = ApprovalId::new();
    store
        .append(
            run,
            EventKind::ApprovalRequested {
                id,
                action: action.into(),
                question: false,
            },
        )
        .unwrap();
    id
}

#[test]
fn approvals_lists_every_request_with_its_outcome_in_order() {
    let store = Store::open_in_memory().unwrap();
    let a = store.create_run(&agent(), "x").unwrap();
    let b = store.create_run(&agent(), "y").unwrap();
    let first = request(&store, a, "git push");
    let second = request(&store, b, "rm -rf build");
    let third = request(&store, a, "send email");
    store
        .append(
            a,
            EventKind::ApprovalResolved {
                id: first,
                approved: true,
                payload: Some("git push origin main".into()),
            },
        )
        .unwrap();
    store
        .append(
            b,
            EventKind::ApprovalResolved {
                id: second,
                approved: false,
                payload: None,
            },
        )
        .unwrap();
    let all = store.approvals().unwrap();
    let got: Vec<_> = all
        .iter()
        .map(|r| (r.id, r.run_id, r.action.as_str(), r.resolved))
        .collect();
    assert_eq!(
        got,
        [
            (first, a, "git push", Some(true)),
            (second, b, "rm -rf build", Some(false)),
            (third, a, "send email", None),
        ]
    );
    assert!(all.iter().all(|r| r.requested_at.year() >= 2026));
    assert_eq!(store.approval(third).unwrap().unwrap().resolved, None);
    assert_eq!(store.approval(ApprovalId::new()).unwrap(), None);
}

#[test]
fn approvals_is_empty_without_requests() {
    let store = Store::open_in_memory().unwrap();
    let run = store.create_run(&agent(), "x").unwrap();
    store
        .append(run, EventKind::AgentLog { line: "hi".into() })
        .unwrap();
    assert!(store.approvals().unwrap().is_empty());
}

#[test]
fn approve_and_reject_with_text_survive_the_control_table() {
    let store = Store::open_in_memory().unwrap();
    let run = store.create_run(&agent(), "x").unwrap();
    let id = ApprovalId::new();
    let cmds = [
        ControlCommand::ApproveWith {
            id,
            payload: "git push origin main".into(),
        },
        ControlCommand::RejectWith {
            id,
            reason: "too risky".into(),
        },
    ];
    for cmd in &cmds {
        store.push_control(run, cmd.clone()).unwrap();
    }
    let taken: Vec<ControlCommand> = store
        .take_control()
        .unwrap()
        .into_iter()
        .map(|(_, _, cmd)| cmd)
        .collect();
    assert_eq!(taken, cmds);
    assert_eq!(cmds[0].approval(), Some(id));
    assert_eq!(cmds[1].approval(), Some(id));
    assert_eq!(ControlCommand::Approve(id).approval(), Some(id));
    assert_eq!(ControlCommand::Reject(id).approval(), Some(id));
    assert_eq!(ControlCommand::Pause.approval(), None);
}
