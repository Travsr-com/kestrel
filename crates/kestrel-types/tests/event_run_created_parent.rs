//! Issue #32, scenario 9: `RunCreated` carries an optional `parent`, and event logs written
//! before the field existed still decode.

use kestrel_types::{Event, EventKind, RunId};
use time::OffsetDateTime;

// 9. Given an old `RunCreated` event JSON without `parent`, when it is decoded, then `parent` is
//    `None`.
#[test]
fn s9_old_run_created_json_without_parent_decodes_with_parent_none() {
    let kind: EventKind =
        serde_json::from_str(r#"{"kind":"run_created","agent":"x"}"#).expect("decode");
    assert_eq!(
        kind,
        EventKind::RunCreated {
            agent: "x".into(),
            parent: None,
        }
    );
}

// 9. The same for a whole stored event, byte for byte as schema v3 wrote it.
#[test]
fn s9_old_stored_run_created_event_decodes_with_parent_none() {
    let stored = r#"{"run_id":"01a1f9a6-a0fa-713c-9285-84f322e2bdf0","seq":1,"ts":"2026-11-20T15:50:09.4132285Z","kind":{"kind":"run_created","agent":"alpha"}}"#;
    let event: Event = serde_json::from_str(stored).expect("decode");
    assert_eq!(event.seq, 1);
    assert_eq!(
        event.run_id.to_string(),
        "01a1f9a6-a0fa-713c-9285-84f322e2bdf0"
    );
    assert_eq!(
        event.kind,
        EventKind::RunCreated {
            agent: "alpha".into(),
            parent: None,
        }
    );
}

// 9. An explicit `null` is also no parent.
#[test]
fn s9_run_created_json_with_null_parent_decodes_with_parent_none() {
    let kind: EventKind =
        serde_json::from_str(r#"{"kind":"run_created","agent":"x","parent":null}"#)
            .expect("decode");
    assert_eq!(
        kind,
        EventKind::RunCreated {
            agent: "x".into(),
            parent: None,
        }
    );
}

// 9. A new event with a parent round-trips, and the parent is in the JSON as the run id string,
//    so the log alone can rebuild the tree.
#[test]
fn s9_run_created_with_a_parent_round_trips() {
    let parent = RunId::new();
    let kind = EventKind::RunCreated {
        agent: "x".into(),
        parent: Some(parent),
    };

    let json = serde_json::to_value(&kind).expect("serialize");
    assert_eq!(json["kind"], "run_created");
    assert_eq!(json["agent"], "x");
    assert_eq!(json["parent"], parent.to_string());
    let back: EventKind = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back, kind);

    let event = Event {
        run_id: RunId::new(),
        seq: 1,
        ts: OffsetDateTime::UNIX_EPOCH,
        kind,
    };
    let text = serde_json::to_string(&event).expect("serialize");
    let back: Event = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back, event);
}

// 9. A root's event (no parent) round-trips too, whichever way `None` is written.
#[test]
fn s9_run_created_without_a_parent_round_trips() {
    let kind = EventKind::RunCreated {
        agent: "x".into(),
        parent: None,
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    let back: EventKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, kind);
}
