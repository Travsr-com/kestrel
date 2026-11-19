//! Claude Code in headless print mode with `stream-json` output.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use kestrel_types::{AgentSpec, EventKind, Runtime};
use serde_json::Value;

use crate::{
    API_KEY_PLACEHOLDER, LaunchPlan, ParsedLine, Result, RunContext, RuntimeAdapter, events,
    parse_json, plan, str_of,
};

/// Keeps the tool name of every `tool_use` id seen so far, because a `tool_result` carries only the id.
#[derive(Debug, Default)]
pub struct ClaudeCodeAdapter {
    tools: Mutex<HashMap<String, String>>,
}

impl ClaudeCodeAdapter {
    fn remember(&self, id: String, name: String) {
        let mut tools = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        tools.insert(id, name);
    }

    fn tool_name(&self, id: Option<&str>) -> String {
        let tools = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        id.and_then(|id| tools.get(id).cloned())
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn assistant(&self, blocks: &[Value]) -> Vec<EventKind> {
        let mut out = Vec::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(text) = str_of(&block["text"]) {
                        out.push(EventKind::AgentMessage { text });
                    }
                }
                Some("tool_use") => {
                    let Some(name) = str_of(&block["name"]) else {
                        continue;
                    };
                    if let Some(id) = str_of(&block["id"]) {
                        self.remember(id, name.clone());
                    }
                    let input = block["input"].clone();
                    out.push(EventKind::ToolCall { name, input });
                }
                // Thinking blocks and anything newer are not events.
                _ => {}
            }
        }
        out
    }

    fn user(&self, blocks: &[Value]) -> Vec<EventKind> {
        blocks
            .iter()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| EventKind::ToolResult {
                name: self.tool_name(b["tool_use_id"].as_str()),
                ok: b["is_error"] != true,
                output: content_text(&b["content"]),
            })
            .collect()
    }
}

/// Tool result content is a string or a list of blocks; text blocks are joined with a newline.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The final `result` line. `is_error` decides success, since an API failure keeps `subtype: "success"`.
/// The reason is `result`, or the `errors` list when an error subtype has no `result`.
fn finished(v: &Value) -> ParsedLine {
    let errors: Vec<&str> = v["errors"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let summary = str_of(&v["result"]).or_else(|| (!errors.is_empty()).then(|| errors.join("\n")));
    ParsedLine::Finished {
        ok: v["is_error"] == false,
        summary,
    }
}

impl RuntimeAdapter for ClaudeCodeAdapter {
    fn runtime(&self) -> Runtime {
        Runtime::ClaudeCode
    }

    fn launch_plan(&self, spec: &AgentSpec, input: &str, ctx: &RunContext) -> Result<LaunchPlan> {
        let argv = [
            "claude",
            "--bare",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            &ctx.model,
            "--permission-mode",
            "bypassPermissions",
            // Ends the options, so an input that starts with `-` is never read as a flag.
            "--",
            input,
        ];
        let env = [
            ("ANTHROPIC_API_KEY", API_KEY_PLACEHOLDER),
            // Claude Code appends `/v1/messages` to the run root itself.
            ("ANTHROPIC_BASE_URL", ctx.gateway_url.as_str()),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ];
        plan(spec, self.runtime(), &argv, &env)
    }

    fn parse_line(&self, line: &str) -> ParsedLine {
        parse_json(line, |v| {
            let blocks = || v["message"]["content"].as_array();
            match v["type"].as_str()? {
                "assistant" => events(self.assistant(blocks()?)),
                "user" => events(self.user(blocks()?)),
                "result" => Some(finished(v)),
                // `system` (init, api_retry) and anything else carry nothing to show.
                _ => None,
            }
        })
    }
}
