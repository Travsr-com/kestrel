//! Edge cases beyond the issue scenarios: spec checks, default image, and lines the fixtures do not hold.

mod common;

use common::{ctx, spec};
use kestrel_agent::{AgentError, ParsedLine, adapter_for};
use kestrel_types::{EventKind, Runtime};
use serde_json::json;

#[test]
fn spec_for_another_runtime_is_rejected() {
    let plan = adapter_for(Runtime::ClaudeCode).launch_plan(&spec(Runtime::OpenCode), "x", &ctx());
    match plan {
        Err(AgentError::RuntimeMismatch { spec, adapter }) => {
            assert_eq!(spec, Runtime::OpenCode);
            assert_eq!(adapter, Runtime::ClaudeCode);
        }
        other => panic!("expected RuntimeMismatch, got {other:?}"),
    }
}

#[test]
fn spec_without_image_uses_the_stock_image() {
    for runtime in Runtime::ALL {
        let mut spec = spec(runtime);
        spec.sandbox.image = None;
        match adapter_for(runtime).launch_plan(&spec, "x", &ctx()) {
            Ok(plan) => assert_eq!(
                plan.image,
                format!("ghcr.io/travsr-com/kestrel-{runtime}:latest")
            ),
            Err(e) => panic!("{runtime}: {e}"),
        }
    }
}

#[test]
fn blank_lines_are_ignored() {
    for runtime in Runtime::ALL {
        for line in ["", "   ", "\t"] {
            assert_eq!(adapter_for(runtime).parse_line(line), ParsedLine::Ignore);
        }
    }
}

#[test]
fn claude_code_tool_result_without_a_known_tool_use_is_named_unknown() {
    let line = json!({
        "type": "user",
        "message": { "content": [
            { "type": "tool_result", "tool_use_id": "toolu_never_seen", "content": "out" }
        ] }
    });
    assert_eq!(
        adapter_for(Runtime::ClaudeCode).parse_line(&line.to_string()),
        ParsedLine::Events(vec![EventKind::ToolResult {
            name: "unknown".to_string(),
            ok: true,
            output: "out".to_string(),
        }])
    );
}

#[test]
fn claude_code_tool_names_do_not_leak_between_adapters() {
    let call = json!({
        "type": "assistant",
        "message": { "content": [ { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} } ] }
    });
    let result = json!({
        "type": "user",
        "message": { "content": [ { "type": "tool_result", "tool_use_id": "toolu_1", "content": [] } ] }
    });
    let first = adapter_for(Runtime::ClaudeCode);
    let _ = first.parse_line(&call.to_string());
    let fresh = adapter_for(Runtime::ClaudeCode);
    assert_eq!(
        fresh.parse_line(&result.to_string()),
        ParsedLine::Events(vec![EventKind::ToolResult {
            name: "unknown".to_string(),
            ok: true,
            output: String::new(),
        }])
    );
}

#[test]
fn claude_code_result_without_reason_has_no_summary() {
    let adapter = adapter_for(Runtime::ClaudeCode);
    assert_eq!(
        adapter.parse_line(r#"{"type":"result","is_error":true}"#),
        ParsedLine::Finished {
            ok: false,
            summary: None
        }
    );
    // Several errors are joined with a newline.
    assert_eq!(
        adapter.parse_line(r#"{"type":"result","is_error":true,"errors":["a","b"]}"#),
        ParsedLine::Finished {
            ok: false,
            summary: Some("a\nb".to_string())
        }
    );
}

#[test]
fn opencode_tool_still_running_and_tool_call_steps_are_ignored() {
    let adapter = adapter_for(Runtime::OpenCode);
    let running = json!({
        "type": "tool_use",
        "part": { "tool": "bash", "state": { "status": "running", "input": {} } }
    });
    assert_eq!(adapter.parse_line(&running.to_string()), ParsedLine::Ignore);
    assert_eq!(
        adapter.parse_line(r#"{"type":"step_finish","part":{"reason":"tool-calls"}}"#),
        ParsedLine::Ignore
    );
    assert_eq!(
        adapter.parse_line(r#"{"type":"step_start","part":{}}"#),
        ParsedLine::Ignore
    );
}

#[test]
fn opencode_error_reason_comes_from_message_or_flat_string() {
    let adapter = adapter_for(Runtime::OpenCode);
    assert_eq!(
        adapter.parse_line(r#"{"type":"error","error":"flat string"}"#),
        ParsedLine::Finished {
            ok: false,
            summary: Some("flat string".to_string())
        }
    );
    assert_eq!(
        adapter.parse_line(r#"{"type":"error"}"#),
        ParsedLine::Finished {
            ok: false,
            summary: None
        }
    );
}
