//! OpenCode in non-interactive `run` mode with `--format json` output.

use kestrel_types::{AgentSpec, EventKind, Runtime};
use serde_json::{Value, json};

use crate::{
    AGENT_NOTE, API_KEY_PLACEHOLDER, CONFIG_DIR, LaunchPlan, ParsedLine, PlanFile, Result,
    RunContext, RuntimeAdapter, Servers, events, parse_json, plan, str_of,
};

#[derive(Debug, Default)]
pub struct OpenCodeAdapter;

/// A finished tool part: the call, then its result. A failed tool's output is its error message.
fn tool_use(part: &Value) -> Option<ParsedLine> {
    let name = str_of(&part["tool"])?;
    let state = &part["state"];
    let (ok, output) = match state["status"].as_str()? {
        "completed" => (true, &state["output"]),
        "error" => (false, &state["error"]),
        _ => return None,
    };
    events(vec![
        EventKind::ToolCall {
            name: name.clone(),
            input: state["input"].clone(),
        },
        EventKind::ToolResult {
            name,
            ok,
            output: str_of(output).unwrap_or_default(),
        },
    ])
}

/// An `error` line ends the run. Its reason is `error.message`, or `error` itself when it is a string.
fn error(v: &Value) -> ParsedLine {
    let e = &v["error"];
    ParsedLine::Finished {
        ok: false,
        summary: str_of(&e["message"]).or_else(|| str_of(e)),
    }
}

impl RuntimeAdapter for OpenCodeAdapter {
    fn runtime(&self) -> Runtime {
        Runtime::OpenCode
    }

    fn launch_plan(&self, spec: &AgentSpec, input: &str, ctx: &RunContext) -> Result<LaunchPlan> {
        let model = format!("anthropic/{}", ctx.model);
        let argv = [
            "opencode",
            "run",
            "--standalone",
            "--format",
            "json",
            "--auto",
            "--model",
            &model,
            // Ends the options, so an input that starts with `-` is never read as a flag.
            "--",
            input,
        ];
        // The Anthropic provider appends `/messages`, so its base URL is the run root plus `/v1`.
        let base_url = format!("{}/v1", ctx.gateway_url.as_str().trim_end_matches('/'));
        let mut config =
            json!({ "providers": { "anthropic": { "settings": { "baseURL": base_url } } } });
        let servers = Servers::of(spec, ctx);
        let mut files = Vec::new();
        if servers.any() {
            let entries =
                servers.entries(|url| json!({ "type": "remote", "url": url, "enabled": true }));
            config["mcp"] = entries.into();
        }
        if servers.kestrel.is_some() {
            files.push(PlanFile {
                name: "kestrel.md".into(),
                contents: AGENT_NOTE.into(),
            });
            config["instructions"] = json!([format!("{CONFIG_DIR}/kestrel.md")]);
        }
        let env = [
            ("ANTHROPIC_API_KEY", API_KEY_PLACEHOLDER),
            ("OPENCODE_CONFIG_CONTENT", &config.to_string()),
        ];
        plan(spec, self.runtime(), &argv, &env, files)
    }

    fn parse_line(&self, line: &str) -> ParsedLine {
        parse_json(line, |v| {
            let part = &v["part"];
            match v["type"].as_str()? {
                "text" => events(vec![EventKind::AgentMessage {
                    text: str_of(&part["text"])?,
                }]),
                "tool_use" => tool_use(part),
                // Steps that end in `tool-calls` continue the run; only `stop` finishes it.
                "step_finish" if part["reason"] == "stop" => Some(ParsedLine::Finished {
                    ok: true,
                    summary: None,
                }),
                "error" => Some(error(v)),
                // `step_start`, other step ends, `reasoning` and anything newer.
                _ => None,
            }
        })
    }
}
