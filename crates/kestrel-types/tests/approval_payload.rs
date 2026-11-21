//! Issue #37, scenario 11 (types side): `ApprovalResolved` carries an optional payload (the edited
//! action for "edit and approve", or a rejection reason), and older events without it decode.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_types::{ApprovalId, EventKind};

#[test]
fn approval_resolved_with_a_payload_round_trips() {
    let kind = EventKind::ApprovalResolved {
        id: ApprovalId::new(),
        approved: true,
        payload: Some("git push origin main".into()),
    };
    let json = serde_json::to_value(&kind).unwrap();
    assert_eq!(json["payload"], "git push origin main");
    assert_eq!(serde_json::from_value::<EventKind>(json).unwrap(), kind);
}

#[test]
fn approval_resolved_without_a_payload_leaves_it_out_and_old_events_still_decode() {
    let id = ApprovalId::new();
    let kind = EventKind::ApprovalResolved {
        id,
        approved: false,
        payload: None,
    };
    let json = serde_json::to_value(&kind).unwrap();
    assert!(json.get("payload").is_none(), "{json}");
    let old = format!(r#"{{"kind":"approval_resolved","id":"{id}","approved":false}}"#);
    assert_eq!(serde_json::from_str::<EventKind>(&old).unwrap(), kind);
}
