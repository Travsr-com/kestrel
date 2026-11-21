//! The JSON contract between `kestrel-server` and the UI.
//!
//! `ui/tests/fixtures/contract.json` is the one sample both sides test against: this test decodes
//! it into the Rust types and encodes it back unchanged, and the UI type-checks and uses the same
//! file. A shape change on either side breaks one of the two.

#![allow(clippy::unwrap_used)]

use kestrel_types::{
    ApiError, Approval, ApprovalStatus, Event, MicroUsd, RunStatus, RunSummary, SandboxTier,
    SuspendReason, TreeNode, WsFrame,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

const CONTRACT: &str = include_str!("../../../ui/tests/fixtures/contract.json");

fn contract() -> Value {
    serde_json::from_str(CONTRACT).unwrap()
}

/// Decodes `value` as `T` and checks that encoding it again gives the same JSON.
fn round_trip<T: Serialize + DeserializeOwned>(value: &Value) -> T {
    let typed: T = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(&serde_json::to_value(&typed).unwrap(), value);
    typed
}

#[test]
fn runs_decode_and_encode_unchanged() {
    let runs: Vec<RunSummary> = round_trip(&contract()["runs"]);
    assert_eq!(runs.len(), 3);
    let root = &runs[0];
    assert_eq!(root.parent, None);
    assert_eq!(root.root, root.id);
    assert_eq!(root.depth, 0);
    assert_eq!(root.status, RunStatus::Running);
    assert_eq!(root.spent, MicroUsd(420_000));
    assert_eq!(root.limit, Some(MicroUsd(2_000_000)));
    assert_eq!(root.sandbox_tier, Some(SandboxTier::Gvisor));
    let child = &runs[1];
    assert_eq!(child.parent, Some(root.id));
    assert_eq!(child.root, root.id);
    assert_eq!(child.depth, 1);
    assert!(matches!(
        child.status,
        RunStatus::Suspended(SuspendReason::Approval(_))
    ));
    assert_eq!(runs[2].limit, None);
    assert_eq!(runs[2].sandbox_tier, None);
}

#[test]
fn tree_decodes_and_encodes_unchanged() {
    let tree: Vec<TreeNode> = round_trip(&contract()["tree"]);
    assert_eq!(tree.len(), 2);
    assert_eq!(tree[1].parent, Some(tree[0].id));
}

#[test]
fn approvals_decode_and_encode_unchanged() {
    let approvals: Vec<Approval> = round_trip(&contract()["approvals"]);
    let statuses: Vec<ApprovalStatus> = approvals.iter().map(|a| a.status).collect();
    assert_eq!(
        statuses,
        [
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Rejected
        ]
    );
}

#[test]
fn events_decode_and_encode_unchanged() {
    let events: Vec<Event> = round_trip(&contract()["events"]);
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [1, 2, 3]);
}

#[test]
fn socket_frames_decode_and_encode_unchanged() {
    let frames: Vec<WsFrame> = round_trip(&contract()["frames"]);
    match &frames[0] {
        WsFrame::Event { cursor, event } => {
            assert_eq!(*cursor, 17);
            assert_eq!(event.seq, 3);
        }
        other => panic!("expected an event frame, got {other:?}"),
    }
    assert_eq!(frames[1], WsFrame::Heartbeat);
}

#[test]
fn error_body_decodes_and_encodes_unchanged() {
    let err: ApiError = round_trip(&contract()["error"]);
    assert_eq!(err.error, "token required");
}

#[test]
fn optional_fields_may_be_missing() {
    let run: RunSummary = serde_json::from_value(serde_json::json!({
        "id": "0b9d8f4e-7a52-4c3e-9a51-5d0c2f1e8a10",
        "agent": "fixer",
        "status": "Queued",
        "root": "0b9d8f4e-7a52-4c3e-9a51-5d0c2f1e8a10",
        "depth": 0,
        "spent": 0,
        "created_at": "2026-11-21T08:00:00Z"
    }))
    .unwrap();
    assert_eq!(run.parent, None);
    assert_eq!(run.limit, None);
    assert_eq!(run.sandbox_tier, None);
}
