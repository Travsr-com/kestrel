//! Scenarios 5 to 8: parsing runtime output lines.

mod common;

use common::fixture;
use kestrel_agent::{ParsedLine, RuntimeAdapter, adapter_for};
use kestrel_types::{EventKind, Runtime};
use proptest::prelude::*;
use serde_json::json;

/// Parses every line with one adapter (adapters may carry state across a stream). Returns the events in
/// order and every `Finished` with the index of the line that produced it.
fn run_stream(
    adapter: &dyn RuntimeAdapter,
    lines: &[String],
) -> (Vec<EventKind>, Vec<(usize, ParsedLine)>) {
    let mut events = Vec::new();
    let mut finished = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        match adapter.parse_line(line) {
            ParsedLine::Events(evs) => events.extend(evs),
            done @ ParsedLine::Finished { .. } => finished.push((i, done)),
            ParsedLine::Ignore => {}
        }
    }
    (events, finished)
}

fn last_line_finished(lines: &[String], finished: &[(usize, ParsedLine)]) -> ParsedLine {
    assert_eq!(
        finished.len(),
        1,
        "exactly one line must finish the run: {finished:?}"
    );
    let (index, done) = &finished[0];
    assert_eq!(
        *index,
        lines.len() - 1,
        "only the last line may finish the run"
    );
    done.clone()
}

fn msg(text: &str) -> EventKind {
    EventKind::AgentMessage {
        text: text.to_string(),
    }
}

fn call(name: &str, input: serde_json::Value) -> EventKind {
    EventKind::ToolCall {
        name: name.to_string(),
        input,
    }
}

fn result(name: &str, ok: bool, output: &str) -> EventKind {
    EventKind::ToolResult {
        name: name.to_string(),
        ok,
        output: output.to_string(),
    }
}

#[test]
fn s5_claude_code_fixture_yields_expected_events_then_finished_ok() {
    let lines = fixture("claude-code/success.jsonl");
    let adapter = adapter_for(Runtime::ClaudeCode);

    assert_eq!(
        adapter.parse_line(&lines[0]),
        ParsedLine::Ignore,
        "system/init"
    );
    let adapter = adapter_for(Runtime::ClaudeCode);
    let (events, finished) = run_stream(adapter.as_ref(), &lines);

    assert_eq!(
        events,
        vec![
            msg("I'll run the tests first."),
            call(
                "Bash",
                json!({ "command": "cargo test", "description": "Run the test suite" })
            ),
            // Array content: text blocks joined with a newline.
            result("Bash", true, "running 3 tests\ntest result: ok. 3 passed"),
            call("Read", json!({ "file_path": "/workspace/src/missing.rs" })),
            // tool_result carries only tool_use_id; the name comes from the earlier tool_use.
            result("Read", false, "File does not exist."),
            // The thinking block before this text is not an event.
            msg("All 3 tests pass. No changes needed."),
        ]
    );
    assert_eq!(
        last_line_finished(&lines, &finished),
        ParsedLine::Finished {
            ok: true,
            summary: Some("All 3 tests pass. No changes needed.".to_string()),
        }
    );
}

#[test]
fn s6_opencode_fixture_yields_expected_events_then_finished_ok() {
    let lines = fixture("opencode/success.jsonl");
    let adapter = adapter_for(Runtime::OpenCode);
    let (events, finished) = run_stream(adapter.as_ref(), &lines);

    assert_eq!(
        events,
        vec![
            msg("I'll run the tests first."),
            call(
                "bash",
                json!({ "command": "cargo test", "description": "Run the test suite" })
            ),
            result("bash", true, "test result: ok. 3 passed"),
            call("read", json!({ "filePath": "/workspace/src/missing.rs" })),
            // A failed tool's output is its error message.
            result("read", false, "File not found: /workspace/src/missing.rs"),
            msg("All 3 tests pass. No changes needed."),
        ]
    );
    // Only the step ending with reason "stop" finishes the run; "tool-calls" steps do not.
    let done = last_line_finished(&lines, &finished);
    assert!(
        matches!(done, ParsedLine::Finished { ok: true, .. }),
        "got {done:?}"
    );
}

fn assert_agent_log(adapter: &dyn RuntimeAdapter, line: &str) {
    assert_eq!(
        adapter.parse_line(line),
        ParsedLine::Events(vec![EventKind::AgentLog {
            line: line.to_string()
        }]),
        "line: {line:?}"
    );
}

#[test]
fn s7_malformed_json_line_becomes_agent_log() {
    let cases = [
        "not json at all",
        "{\"type\":\"assistant\"",
        "{type: assistant}",
        "Error: When using --print, --output-format=stream-json requires --verbose",
        "\u{1b}[31mpanic\u{1b}[0m {",
    ];
    for runtime in Runtime::ALL {
        let adapter = adapter_for(runtime);
        for line in cases {
            assert_agent_log(adapter.as_ref(), line);
        }
    }
}

/// Valid JSON in an unexpected shape has no fixed result, but it must not panic.
#[test]
fn s7_valid_json_with_wrong_shape_never_panics() {
    let cases = [
        "null",
        "42",
        "[]",
        "\"text\"",
        "{}",
        "{\"type\":null}",
        "{\"type\":\"assistant\"}",
        "{\"type\":\"assistant\",\"message\":null}",
        "{\"type\":\"assistant\",\"message\":{\"content\":\"plain\"}}",
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\"}]}}",
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":7}]}}",
        "{\"type\":\"result\"}",
        "{\"type\":\"result\",\"is_error\":\"yes\",\"errors\":[1,2]}",
        "{\"type\":\"text\",\"part\":null}",
        "{\"type\":\"tool_use\",\"part\":{\"state\":{\"status\":\"completed\"}}}",
        "{\"type\":\"step_finish\",\"part\":{\"reason\":5}}",
        "{\"type\":\"error\",\"error\":\"flat string\"}",
        "{\"type\":\"error\"}",
    ];
    for runtime in Runtime::ALL {
        let adapter = adapter_for(runtime);
        for line in cases {
            let _ = adapter.parse_line(line);
        }
    }
}

fn every_fixture_line() -> Vec<String> {
    [
        "claude-code/success.jsonl",
        "claude-code/error-api.jsonl",
        "claude-code/error-max-turns.jsonl",
        "opencode/success.jsonl",
        "opencode/error.jsonl",
    ]
    .into_iter()
    .flat_map(fixture)
    .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn s7_proptest_arbitrary_string_never_panics_and_malformed_is_agent_log(line in any::<String>()) {
        for runtime in Runtime::ALL {
            let parsed = adapter_for(runtime).parse_line(&line);
            let malformed = serde_json::from_str::<serde_json::Value>(&line).is_err();
            if malformed && !line.trim().is_empty() {
                prop_assert_eq!(
                    parsed,
                    ParsedLine::Events(vec![EventKind::AgentLog { line: line.clone() }])
                );
            }
        }
    }

    #[test]
    fn s7_proptest_truncated_fixture_line_is_agent_log(
        pick in any::<prop::sample::Index>(),
        cut in any::<prop::sample::Index>(),
    ) {
        let lines = every_fixture_line();
        let full = pick.get(&lines);
        let ends: Vec<usize> = full.char_indices().map(|(i, _)| i).filter(|&i| i > 0).collect();
        let line = &full[..*cut.get(&ends)];
        prop_assume!(serde_json::from_str::<serde_json::Value>(line).is_err());
        for runtime in Runtime::ALL {
            prop_assert_eq!(
                adapter_for(runtime).parse_line(line),
                ParsedLine::Events(vec![EventKind::AgentLog { line: line.to_string() }])
            );
        }
    }
}

fn last_line(rel: &str) -> String {
    match fixture(rel).pop() {
        Some(l) => l,
        None => panic!("fixture {rel} is empty"),
    }
}

#[test]
fn s8_claude_code_error_result_is_finished_not_ok_with_reason() {
    let adapter = adapter_for(Runtime::ClaudeCode);
    // subtype stays "success"; is_error decides.
    assert_eq!(
        adapter.parse_line(&last_line("claude-code/error-api.jsonl")),
        ParsedLine::Finished {
            ok: false,
            summary: Some("API Error: Connection refused (ECONNREFUSED)".to_string()),
        }
    );
    // Error subtypes carry no `result`; the reason is in `errors`.
    assert_eq!(
        adapter.parse_line(&last_line("claude-code/error-max-turns.jsonl")),
        ParsedLine::Finished {
            ok: false,
            summary: Some("Reached maximum number of turns (1)".to_string()),
        }
    );
}

#[test]
fn s8_opencode_error_event_is_finished_not_ok_with_reason() {
    let adapter = adapter_for(Runtime::OpenCode);
    assert_eq!(
        adapter.parse_line(&last_line("opencode/error.jsonl")),
        ParsedLine::Finished {
            ok: false,
            summary: Some("Provider returned 401: invalid x-api-key".to_string()),
        }
    );
}
