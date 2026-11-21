//! Issue #41: an approval can be a question for a person (`ask_human`), answered with text
//! instead of approved as an action. The flag is optional on the wire, so older logs still read.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_types::{Approval, ApprovalId, ApprovalStatus, EventKind, RunId};
use serde_json::json;

#[test]
fn an_approval_request_is_an_action_unless_marked_as_a_question() {
    let id = ApprovalId::new();
    let old = json!({ "kind": "approval_requested", "id": id, "action": "git push" });
    let kind: EventKind = serde_json::from_value(old).unwrap();
    assert_eq!(
        kind,
        EventKind::ApprovalRequested {
            id,
            action: "git push".into(),
            question: false,
        }
    );
    // An action is written without the flag, exactly as before.
    let written = serde_json::to_value(&kind).unwrap();
    assert!(written.get("question").is_none(), "{written}");
}

#[test]
fn a_question_round_trips_with_its_flag() {
    let kind = EventKind::ApprovalRequested {
        id: ApprovalId::new(),
        action: "Which branch?".into(),
        question: true,
    };
    let written = serde_json::to_value(&kind).unwrap();
    assert_eq!(written["question"], json!(true));
    let read: EventKind = serde_json::from_value(written).unwrap();
    assert_eq!(read, kind);
}

#[test]
fn the_api_says_whether_an_approval_is_a_question() {
    let approval = Approval {
        id: ApprovalId::new(),
        run_id: RunId::new(),
        action: "Which branch?".into(),
        status: ApprovalStatus::Pending,
        requested_at: time::OffsetDateTime::UNIX_EPOCH,
        question: true,
    };
    let written = serde_json::to_value(&approval).unwrap();
    assert_eq!(written["question"], json!(true));
    // A body without the field (an older server) reads as an action.
    let mut older = written.clone();
    older.as_object_mut().unwrap().remove("question");
    let read: Approval = serde_json::from_value(older).unwrap();
    assert!(!read.question);
}
