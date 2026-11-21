//! The Kestrel tools for agents in a sandbox, served over MCP at `/r/{run_token}/mcp`.
//!
//! A sandbox's only way out is the gateway, so the tools live next to the LLM routes, behind the
//! same run token. The protocol is MCP's streamable HTTP ([`MCP_PROTOCOL_VERSION`]): one JSON-RPC
//! message per `POST`. `initialize`, `ping` and `tools/list` answer with JSON. `tools/call` answers
//! with an event stream when the client accepts one, and sends a keepalive comment every 15 s while
//! a long call (`join`, `ask_human`) waits, so the connection through the bridge stays up.
//!
//! * **Who acts**: the run is the one the token resolves to, never one named in the request.
//! * **What it may call**: [`ToolHost::tools`] lists the tools the run's capability allows.
//!   `tools/list` shows only those; calling another is a tool error "not permitted", and the host
//!   records the denial.
//! * **Calls survive a dropped connection**: each call runs in its own task, keyed by the run and
//!   the JSON-RPC id. Calling again with the same id waits for the same result, so a client that
//!   reconnects gets it without a second spawn or a second wait. Results are kept for an hour.
//! * **Limits**: bodies up to 1 MiB (413 above), and per run a burst of 20 tool calls refilled at 10
//!   per second (429 above). An unknown token is 401 and a finished run 410; neither echoes it.

use std::collections::HashMap;
use std::convert::Infallible;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use kestrel_types::{MicroUsd, RunId};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::AppState;

/// The MCP protocol version this server speaks (streamable HTTP).
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Largest MCP request body.
const MAX_BYTES: usize = 1024 * 1024;
/// How often a waiting call sends a keepalive comment.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// How long a finished call's result is kept for a client that calls again.
const KEEP_RESULT: Duration = Duration::from_secs(60 * 60);
/// Tool calls a run may make at once, and how many come back per second.
const BURST: f64 = 20.0;
const REFILL_PER_SECOND: f64 = 10.0;

/// A Kestrel tool a sandbox can call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KestrelTool {
    Spawn,
    Join,
    AskHuman,
}

impl KestrelTool {
    pub const ALL: [KestrelTool; 3] =
        [KestrelTool::Spawn, KestrelTool::Join, KestrelTool::AskHuman];

    /// Its MCP name, which is also its name in a capability's tools.
    pub fn name(self) -> &'static str {
        match self {
            KestrelTool::Spawn => "kestrel.spawn",
            KestrelTool::Join => "kestrel.join",
            KestrelTool::AskHuman => "kestrel.ask_human",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    fn describe(self) -> Value {
        let (description, schema) = match self {
            KestrelTool::Spawn => (
                "Start a child run of another agent with its own budget, carved out of yours. \
                 Returns the child's run id; with wait, also its result.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent": { "type": "string", "description": "Name of an agent you may spawn." },
                        "input": { "type": "string", "description": "The child's task." },
                        "budget_usd": {
                            "type": ["string", "number"],
                            "description": "Dollars to give the child, such as \"0.50\". Default: the smaller of its spec's budget and half of what you have left."
                        },
                        "wait": { "type": "boolean", "description": "Wait for the child to finish and return its result." }
                    },
                    "required": ["agent", "input"]
                }),
            ),
            KestrelTool::Join => (
                "Wait for child runs to finish and get their results. Without children, waits for all of yours.",
                json!({
                    "type": "object",
                    "properties": {
                        "children": { "type": "array", "items": { "type": "string" }, "description": "Child run ids." },
                        "mode": { "type": "string", "enum": ["all", "any"], "description": "Wait for all (default) or the first to finish." },
                        "timeout_s": { "type": "number", "minimum": 0, "description": "Give up waiting after this many seconds." }
                    }
                }),
            ),
            KestrelTool::AskHuman => (
                "Ask a person a question and wait for the answer. Your run waits while they decide.",
                json!({
                    "type": "object",
                    "properties": { "question": { "type": "string", "description": "What to ask." } },
                    "required": ["question"]
                }),
            ),
        };
        json!({ "name": self.name(), "description": description, "inputSchema": schema })
    }
}

/// The run has finished: it can call nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunGone;

/// `kestrel.spawn` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCall {
    pub agent: String,
    pub input: String,
    pub budget: Option<MicroUsd>,
    pub wait: bool,
}

/// What `kestrel.join` waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinWait {
    All,
    Any,
}

/// `kestrel.join` arguments. No children means every child of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCall {
    pub children: Vec<RunId>,
    pub wait: JoinWait,
    pub timeout: Option<Duration>,
}

/// Does what the tools ask, for the run the token resolved to. An `Err` is a reason the model reads
/// (budget, depth, an unknown agent), so it can adapt.
#[async_trait]
pub trait ToolHost: Send + Sync {
    /// The tools `run` may call, or [`RunGone`] once it has finished.
    async fn tools(&self, run: RunId) -> Result<Vec<KestrelTool>, RunGone>;
    async fn spawn(&self, run: RunId, call: SpawnCall) -> Result<Value, String>;
    async fn join(&self, run: RunId, call: JoinCall) -> Result<Value, String>;
    /// Asks a person; the answer text once they decide.
    async fn ask_human(&self, run: RunId, question: String) -> Result<String, String>;
    /// Records that `run` called `tool`, which it may not.
    async fn denied(&self, run: RunId, tool: &str);
}

/// A host with no tools, for a gateway that serves only the LLM routes.
pub struct NoTools;

#[async_trait]
impl ToolHost for NoTools {
    async fn tools(&self, _: RunId) -> Result<Vec<KestrelTool>, RunGone> {
        Ok(Vec::new())
    }
    async fn spawn(&self, _: RunId, _: SpawnCall) -> Result<Value, String> {
        Err("no tools are served here".into())
    }
    async fn join(&self, _: RunId, _: JoinCall) -> Result<Value, String> {
        Err("no tools are served here".into())
    }
    async fn ask_human(&self, _: RunId, _: String) -> Result<String, String> {
        Err("no tools are served here".into())
    }
    async fn denied(&self, _: RunId, _: &str) {}
}

/// A call's result once it is done, shared by every request with the same run and id.
type CallResult = watch::Receiver<Option<Value>>;

/// Calls in flight or recently finished, and each run's call allowance.
#[derive(Default)]
pub(crate) struct McpState {
    calls: Mutex<HashMap<(RunId, String), CallResult>>,
    buckets: Mutex<HashMap<RunId, (f64, Instant)>>,
}

impl McpState {
    /// Takes one tool call from `run`'s allowance; `false` when it has none left.
    fn allow(&self, run: RunId) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let (tokens, at) = buckets.entry(run).or_insert((BURST, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * REFILL_PER_SECOND).min(BURST);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A JSON-RPC error response.
fn rpc_error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    let body = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    (status, Json(body)).into_response()
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A tool result for the model: its text, structured content for JSON, and whether it failed.
fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(Value::String(text)) => {
            json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "structuredContent": value,
            "isError": false
        }),
        Err(reason) => json!({ "content": [{ "type": "text", "text": reason }], "isError": true }),
    }
}

/// `POST /r/{run_token}/mcp`.
pub(crate) async fn post(
    state: Arc<AppState>,
    token: String,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(run) = state.deps.resolver.resolve(&token).await else {
        return rpc_error(
            StatusCode::UNAUTHORIZED,
            Value::Null,
            -32001,
            "unknown run token",
        );
    };
    // Read the body only once the caller is known.
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BYTES).await else {
        return rpc_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            Value::Null,
            -32002,
            "request body is larger than 1 MiB",
        );
    };
    let Ok(tools) = state.deps.tools.tools(run).await else {
        return rpc_error(
            StatusCode::GONE,
            Value::Null,
            -32003,
            "the run has finished",
        );
    };
    let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
        return rpc_error(StatusCode::BAD_REQUEST, Value::Null, -32700, "parse error");
    };
    let Some(object) = message.as_object() else {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32600,
            "invalid request: send one JSON-RPC object",
        );
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            id,
            -32600,
            "invalid request: no method",
        );
    };
    if id.is_null() {
        // A notification, such as `notifications/initialized`: nothing to answer.
        return StatusCode::ACCEPTED.into_response();
    }
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    match method {
        "initialize" => Json(rpc_result(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "kestrel", "version": env!("CARGO_PKG_VERSION") }
            }),
        ))
        .into_response(),
        "ping" => Json(rpc_result(id, json!({}))).into_response(),
        "tools/list" => {
            let listed: Vec<Value> = tools.iter().map(|t| t.describe()).collect();
            Json(rpc_result(id, json!({ "tools": listed }))).into_response()
        }
        "tools/call" => call(&state, run, &tools, id, &params, accepts_events(&headers)).await,
        _ => rpc_error(StatusCode::OK, id, -32601, "method not found"),
    }
}

/// `GET /r/{run_token}/mcp`: this server sends nothing unasked, so it offers no stream here.
pub(crate) async fn get() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response()
}

fn accepts_events(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.contains("text/event-stream"))
}

/// What a checked `tools/call` will do.
enum Task {
    Spawn(SpawnCall),
    Join(JoinCall),
    Ask(String),
}

async fn call(
    state: &Arc<AppState>,
    run: RunId,
    tools: &[KestrelTool],
    id: Value,
    params: &Value,
    events: bool,
) -> Response {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return rpc_error(
            StatusCode::OK,
            id,
            -32602,
            "invalid params: name is required",
        );
    };
    let Some(tool) = KestrelTool::from_name(name) else {
        return rpc_error(StatusCode::OK, id, -32602, "invalid params: unknown tool");
    };
    if !state.mcp.allow(run) {
        return rpc_error(
            StatusCode::TOO_MANY_REQUESTS,
            id,
            -32000,
            "too many tool calls, slow down",
        );
    }
    if !tools.contains(&tool) {
        state.deps.tools.denied(run, tool.name()).await;
        let reason = format!(
            "not permitted: this run's capabilities do not include {}",
            tool.name()
        );
        return Json(rpc_result(id, tool_result(Err(reason)))).into_response();
    }
    let empty = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty);
    let task = match check(tool, arguments) {
        Ok(task) => task,
        Err(problem) => {
            return rpc_error(
                StatusCode::OK,
                id,
                -32602,
                &format!("invalid params: {problem}"),
            );
        }
    };
    let mut result = start(state, run, &id, task);
    let wait = async move {
        loop {
            if let Some(value) = result.borrow_and_update().clone() {
                return value;
            }
            if result.changed().await.is_err() {
                return tool_result(Err("the call failed".into()));
            }
        }
    };
    if !events {
        return Json(rpc_result(id, wait.await)).into_response();
    }
    let stream = futures::stream::once(async move {
        let message = rpc_result(id, wait.await);
        Ok::<_, Infallible>(Event::default().event("message").data(message.to_string()))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE))
        .into_response()
}

/// The call for `(run, id)`: the one already running or done, or a new one in its own task, so a
/// client that goes away does not cancel it.
fn start(
    state: &Arc<AppState>,
    run: RunId,
    id: &Value,
    task: Task,
) -> watch::Receiver<Option<Value>> {
    let key = (run, id.to_string());
    let mut calls = state
        .mcp
        .calls
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = calls.get(&key) {
        return existing.clone();
    }
    let (tx, rx) = watch::channel(None);
    calls.insert(key.clone(), rx.clone());
    drop(calls);
    let state = state.clone();
    tokio::spawn(async move {
        let host = &state.deps.tools;
        let outcome = match task {
            Task::Spawn(call) => host.spawn(run, call).await,
            Task::Join(call) => host.join(run, call).await,
            Task::Ask(question) => host.ask_human(run, question).await.map(Value::String),
        };
        let _ = tx.send(Some(tool_result(outcome)));
        tokio::time::sleep(KEEP_RESULT).await;
        state
            .mcp
            .calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
    });
    rx
}

/// Checks a tool's arguments.
fn check(tool: KestrelTool, args: &Value) -> Result<Task, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    match tool {
        KestrelTool::Spawn => {
            let agent = text("agent")
                .filter(|a| !a.trim().is_empty())
                .ok_or("agent is required")?;
            let input = text("input").ok_or("input is required")?;
            let budget = match args.get("budget_usd") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(dollars(s)?),
                Some(Value::Number(n)) => Some(dollars(&n.to_string())?),
                Some(_) => return Err("budget_usd must be dollars, such as \"0.50\"".into()),
            };
            let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(false);
            Ok(Task::Spawn(SpawnCall {
                agent,
                input,
                budget,
                wait,
            }))
        }
        KestrelTool::Join => {
            let mut children = Vec::new();
            if let Some(list) = args.get("children") {
                let list = list
                    .as_array()
                    .ok_or("children must be a list of run ids")?;
                for child in list {
                    let id = child.as_str().and_then(|c| RunId::from_str(c).ok());
                    children.push(id.ok_or("children must be a list of run ids")?);
                }
            }
            let wait = match args.get("mode").and_then(Value::as_str) {
                None | Some("all") => JoinWait::All,
                Some("any") => JoinWait::Any,
                Some(_) => return Err("mode must be all or any".into()),
            };
            let timeout = match args.get("timeout_s") {
                None | Some(Value::Null) => None,
                Some(t) => {
                    let secs = t
                        .as_f64()
                        .filter(|s| s.is_finite() && *s >= 0.0)
                        .ok_or("timeout_s must be a number of seconds, 0 or more")?;
                    Some(Duration::from_secs_f64(secs))
                }
            };
            Ok(Task::Join(JoinCall {
                children,
                wait,
                timeout,
            }))
        }
        KestrelTool::AskHuman => {
            let question = text("question")
                .filter(|q| !q.trim().is_empty())
                .ok_or("question is required")?;
            Ok(Task::Ask(question))
        }
    }
}

fn dollars(text: &str) -> Result<MicroUsd, String> {
    MicroUsd::parse_decimal(text)
        .ok_or_else(|| "budget_usd must be dollars, such as \"0.50\"".to_string())
}
