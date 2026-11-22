//! Issue #95: messages a person sends to a running agent. What a message may hold, which runs
//! take one, and the two events that record it.

#![allow(clippy::unwrap_used)]

use kestrel_types::{
    AgentSpec, EventKind, MAX_MESSAGE_BYTES, MESSAGES_TOOL, RunSummary, message_problem,
};
use serde_json::json;

fn spec(mcp: &str) -> AgentSpec {
    AgentSpec::from_yaml(&format!(
        "name: a\nruntime: claude-code\nrole: r\ncapabilities: {{ mcp: [{mcp}] }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
         budget: {{ usd: \"1\", wall_clock: 1h }}\n"
    ))
    .unwrap()
}

#[test]
fn the_tool_and_the_limit_are_named_once() {
    assert_eq!(MESSAGES_TOOL, "kestrel.messages");
    assert_eq!(MAX_MESSAGE_BYTES, 16 * 1024);
}

#[test]
fn a_message_is_text_of_1_to_16_kib() {
    assert_eq!(message_problem("also fix the docs"), None);
    // Lines, tabs and any language are text.
    assert_eq!(
        message_problem("first\nsecond\r\n\tthird, \u{00e9}t\u{00e9}"),
        None
    );
    assert_eq!(message_problem(&"x".repeat(MAX_MESSAGE_BYTES)), None);

    for empty in ["", "   ", "\n\t"] {
        let why = message_problem(empty).unwrap();
        assert!(why.contains("empty"), "{empty:?}: {why}");
    }
    let why = message_problem(&"x".repeat(MAX_MESSAGE_BYTES + 1)).unwrap();
    assert!(why.contains("16384"), "{why}");
    // Counted in bytes, not characters.
    let wide = "\u{00e9}".repeat(MAX_MESSAGE_BYTES / 2 + 1);
    assert!(message_problem(&wide).is_some());
}

#[test]
fn a_message_holds_no_control_characters_but_lines_and_tabs() {
    for bad in ["ring \u{7}", "\u{1b}[31mred", "nul \0 byte", "del \u{7f}"] {
        let why = message_problem(bad).unwrap();
        assert!(why.contains("text"), "{bad:?}: {why}");
    }
}

#[test]
fn a_spec_takes_messages_when_it_grants_kestrel_messages() {
    assert!(spec("kestrel.messages").takes_messages());
    assert!(spec("kestrel.join, kestrel.messages").takes_messages());
    assert!(!spec("kestrel.join").takes_messages());
    assert!(!spec("").takes_messages());
}

#[test]
fn message_events_have_a_stable_json_shape() {
    let sent = EventKind::MessageSent {
        text: "also fix the docs".into(),
    };
    assert_eq!(
        serde_json::to_value(&sent).unwrap(),
        json!({ "kind": "message_sent", "text": "also fix the docs" })
    );
    let delivered = EventKind::MessageDelivered { seqs: vec![4, 7] };
    assert_eq!(
        serde_json::to_value(&delivered).unwrap(),
        json!({ "kind": "message_delivered", "seqs": [4, 7] })
    );
    for kind in [sent, delivered] {
        let back: EventKind = serde_json::from_value(serde_json::to_value(&kind).unwrap()).unwrap();
        assert_eq!(back, kind);
    }
}

#[test]
fn a_run_summary_from_an_older_server_takes_no_messages() {
    let run: RunSummary = serde_json::from_value(json!({
        "id": "0b9d8f4e-7a52-4c3e-9a51-5d0c2f1e8a10",
        "agent": "fixer",
        "status": "Running",
        "root": "0b9d8f4e-7a52-4c3e-9a51-5d0c2f1e8a10",
        "depth": 0,
        "spent": 0,
        "created_at": "2026-11-21T08:00:00Z"
    }))
    .unwrap();
    assert!(!run.takes_messages);
}
