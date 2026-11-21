//! Agent runtime adapters: claude-code and opencode.
//!
//! An adapter turns an [`AgentSpec`] and a task into a [`LaunchPlan`] for a headless runtime, and turns
//! each line of the runtime's streaming JSON output into a [`ParsedLine`].
//!
//! A runtime gets the run's MCP servers on the gateway when its capability grants their tools:
//! `kestrel` at `{run root}/mcp` for `kestrel.*` tools (#42) and `travsr` at `{run root}/travsr` for
//! `travsr.*` tools (#44). The run token in the path is their only credential. Config the runtime
//! reads from a file goes in [`LaunchPlan::files`], which the engine mounts read-only at
//! [`CONFIG_DIR`], outside the workspace, so nothing of it is committed.

use std::collections::BTreeMap;

use kestrel_types::{AgentSpec, EventKind, RunId, Runtime};
use serde_json::Value;
use url::Url;

mod claude_code;
mod opencode;

pub use claude_code::ClaudeCodeAdapter;
pub use opencode::OpenCodeAdapter;

/// Value given to the runtime as its API key. The gateway holds the real provider key, so the sandbox
/// never sees one.
pub const API_KEY_PLACEHOLDER: &str = kestrel_types::GATEWAY_KEY_PLACEHOLDER;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("spec runtime `{spec}` does not match adapter runtime `{adapter}`")]
    RuntimeMismatch { spec: Runtime, adapter: Runtime },
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// What an adapter needs to know about the run it is launching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContext {
    pub run_id: RunId,
    /// Per-run gateway URL, shaped `.../r/{token}`.
    pub gateway_url: Url,
    /// Model name as the gateway expects it.
    pub model: String,
}

/// How to start the runtime inside a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub image: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Files the runtime reads, found in the sandbox at [`CONFIG_DIR`]`/{name}`.
    pub files: Vec<PlanFile>,
}

/// One file of a [`LaunchPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFile {
    /// A plain file name, no directories.
    pub name: String,
    pub contents: String,
}

/// Where a sandbox finds the files of its plan: read-only, outside `/workspace`.
pub const CONFIG_DIR: &str = "/kestrel";

/// The part of the agent note for a run that may spawn (`kestrel.spawn`).
const SPAWN_NOTE: &str = "\
You can hand parts of your task to other agents. Do it only when the work splits into parts that
can go on at the same time.

* kestrel.spawn(agent, input, budget_usd?, wait?) starts a child run of another agent. Its budget
  comes out of yours: what you give it you cannot spend, and what it does not spend comes back
  when it ends. Without budget_usd it gets half of what you have left, at most its own budget.
  With wait set, it also waits for the child and returns how it ended.
* A child starts from nothing: give it a complete, self-contained task.
* Each child works in its own workspace, set by its agent spec: usually a fresh clone of the
  original repository, never your copy. It sees none of your changes, committed or not, and its
  changes do not reach yours: put what it needs in its input, and ask for what you need back in
  its summary.
* Few well-scoped children beat many small ones: each one costs a sandbox and its own model calls.
";

/// The part of the agent note for a run that may spawn and also holds `kestrel.join`.
const JOIN_NOTE: &str = "\
* kestrel.join(children?, mode?, timeout_s?) waits for children and returns how each ended:
  completed with its summary, failed with a reason, cancelled, or timed out. Waiting costs no
  model calls.
";

/// The part of the agent note for a run granted `kestrel.ask_human`.
const ASK_NOTE: &str = "\
* kestrel.ask_human(question) asks a person and waits for the answer. Ask only what you cannot
  find out yourself.
";

/// What an agent is told about the Kestrel tools it was offered: spawning when it may spawn,
/// joining when it may also join, asking a person when it may ask. `None` when it may neither
/// spawn nor ask, since `kestrel.join` alone has no children to wait for. No token, URL or path.
pub fn agent_note(spawn: bool, join: bool, ask: bool) -> Option<String> {
    if !spawn && !ask {
        return None;
    }
    let mut note = String::from("# Kestrel tools\n\n");
    if spawn {
        note.push_str(SPAWN_NOTE);
        if join {
            note.push_str(JOIN_NOTE);
        }
    }
    if ask {
        note.push_str(ASK_NOTE);
    }
    Some(note)
}

/// The meaning of one line of runtime output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedLine {
    Events(Vec<EventKind>),
    Finished { ok: bool, summary: Option<String> },
    Ignore,
}

pub trait RuntimeAdapter: Send + Sync {
    fn runtime(&self) -> Runtime;
    fn launch_plan(&self, spec: &AgentSpec, input: &str, ctx: &RunContext) -> Result<LaunchPlan>;
    /// Never panics, whatever the line holds.
    fn parse_line(&self, line: &str) -> ParsedLine;
}

/// A fresh adapter for `r`. Use one adapter per run: it may keep state across lines of one stream.
pub fn adapter_for(r: Runtime) -> Box<dyn RuntimeAdapter> {
    match r {
        Runtime::ClaudeCode => Box::new(ClaudeCodeAdapter::default()),
        Runtime::OpenCode => Box::new(OpenCodeAdapter),
    }
}

/// The run's MCP servers on the gateway, from what its capability grants.
struct Servers {
    /// `{run root}/mcp`, when any `kestrel.*` tool is granted.
    kestrel: Option<String>,
    /// `{run root}/travsr`, when any `travsr.*` tool is granted.
    travsr: Option<String>,
    /// The granted tools as Claude Code names MCP tools: `mcp__{server}__{tool}`, with every
    /// character other than a letter, digit, `_` or `-` made `_`. Each once.
    tools: Vec<String>,
    /// `kestrel.spawn` is offered.
    spawn: bool,
    /// `kestrel.join` is offered.
    join: bool,
    /// `kestrel.ask_human` is offered.
    ask: bool,
}

impl Servers {
    fn of(spec: &AgentSpec, ctx: &RunContext) -> Servers {
        let caps = &spec.capabilities;
        let mut tools: Vec<String> = Vec::new();
        let (mut kestrel, mut travsr) = (false, false);
        let (mut spawn, mut join, mut ask) = (false, false, false);
        for name in &caps.mcp {
            let tool = if let Some(tool) = name.strip_prefix("kestrel.") {
                match tool {
                    // Spawning also needs spawn rights, as the gateway checks.
                    "spawn" if caps.spawn.as_ref().is_some_and(|s| s.may_spawn()) => spawn = true,
                    "ask_human" => ask = true,
                    "join" => join = true,
                    // The gateway has no other Kestrel tool, so it would never list this one.
                    _ => continue,
                }
                kestrel = true;
                format!("mcp__kestrel__{}", claude_name(name))
            } else if let Some(tool) = name.strip_prefix("travsr.") {
                // Named without the `travsr.` prefix, on the assumption that the Travsr route
                // (#44) lists its tools as Travsr names them. Only a name: the flag it goes in
                // restricts nothing, so a mismatch hides no tool.
                travsr = true;
                format!("mcp__travsr__{}", claude_name(tool))
            } else {
                continue;
            };
            if !tools.contains(&tool) {
                tools.push(tool);
            }
        }
        Servers {
            kestrel: kestrel.then(|| endpoint(&ctx.gateway_url, "mcp")),
            travsr: travsr.then(|| endpoint(&ctx.gateway_url, "travsr")),
            tools,
            spawn,
            join,
            ask,
        }
    }

    fn any(&self) -> bool {
        self.kestrel.is_some() || self.travsr.is_some()
    }

    /// The agent note for the Kestrel tools offered, see [`agent_note`].
    fn note(&self) -> Option<String> {
        agent_note(self.spawn, self.join, self.ask)
    }

    /// The servers by name, each to `entry(url)`.
    fn entries(&self, entry: impl Fn(&str) -> Value) -> serde_json::Map<String, Value> {
        [("kestrel", &self.kestrel), ("travsr", &self.travsr)]
            .into_iter()
            .filter_map(|(name, url)| url.as_ref().map(|u| (name.to_string(), entry(u))))
            .collect()
    }
}

fn claude_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `{run root}/{segment}`, built as a URL: the token stays as escaped as it was, and a run root
/// with a trailing slash gets no double slash.
fn endpoint(root: &Url, segment: &str) -> String {
    let mut url = root.clone();
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().push(segment);
    }
    url.to_string()
}

/// A plan for `runtime`. The image is the spec's sandbox image, or the stock image for `runtime` when the
/// spec names none. The env holds only `env`: nothing is copied from the host environment.
fn plan(
    spec: &AgentSpec,
    runtime: Runtime,
    argv: &[&str],
    env: &[(&str, &str)],
    files: Vec<PlanFile>,
) -> Result<LaunchPlan> {
    if spec.runtime != runtime {
        return Err(AgentError::RuntimeMismatch {
            spec: spec.runtime,
            adapter: runtime,
        });
    }
    let image = spec.sandbox.image.clone();
    Ok(LaunchPlan {
        image: image.unwrap_or_else(|| format!("ghcr.io/travsr-com/kestrel-{runtime}:latest")),
        argv: argv.iter().map(|a| a.to_string()).collect(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        files,
    })
}

/// Parses `line` as a JSON object and hands it to `f`. Blank lines are ignored and anything that is not
/// JSON becomes an [`EventKind::AgentLog`], so nothing the runtime prints is lost.
fn parse_json(line: &str, f: impl FnOnce(&Value) -> Option<ParsedLine>) -> ParsedLine {
    if line.trim().is_empty() {
        return ParsedLine::Ignore;
    }
    match serde_json::from_str::<Value>(line) {
        Ok(value) => f(&value).unwrap_or(ParsedLine::Ignore),
        Err(_) => ParsedLine::Events(vec![EventKind::AgentLog {
            line: line.to_string(),
        }]),
    }
}

/// `Events(events)`, or `None` when there are none.
fn events(events: Vec<EventKind>) -> Option<ParsedLine> {
    (!events.is_empty()).then_some(ParsedLine::Events(events))
}

fn str_of(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}
