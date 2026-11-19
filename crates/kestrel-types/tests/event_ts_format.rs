//! Event timestamps are stored as RFC 3339 strings, so stored events stay readable and sortable.

use kestrel_types::{Event, EventKind, RunId};
use time::OffsetDateTime;

#[test]
fn ts_serializes_as_rfc3339_string() {
    let event = Event {
        run_id: RunId::new(),
        seq: 1,
        ts: OffsetDateTime::UNIX_EPOCH,
        kind: EventKind::AgentLog { line: "hi".into() },
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["ts"], "1970-01-01T00:00:00Z");
    let back: Event = serde_json::from_value(json).unwrap();
    assert_eq!(back, event);
}
