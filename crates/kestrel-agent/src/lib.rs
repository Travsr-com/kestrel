//! Agent runtime adapters: claude-code and opencode.
//!
//! An adapter turns an [`AgentSpec`] and a task into a [`LaunchPlan`] for a headless runtime, and turns
//! each line of the runtime's streaming JSON output into a [`ParsedLine`].

use std::collections::BTreeMap;

use kestrel_types::{AgentSpec, EventKind, RunId, Runtime};
use url::Url;

mod claude_code;
mod opencode;

pub use claude_code::ClaudeCodeAdapter;
pub use opencode::OpenCodeAdapter;

/// Value given to the runtime as its API key. The gateway holds the real provider key, so the sandbox
/// never sees one.
pub const API_KEY_PLACEHOLDER: &str = "kestrel-gateway-placeholder";

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
    let _ = r;
    todo!()
}
