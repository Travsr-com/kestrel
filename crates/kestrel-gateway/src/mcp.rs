//! The Kestrel tools for agents in a sandbox, served over MCP at `/r/{run_token}/mcp`.
//!
//! A sandbox's only way out is the gateway, so the tools live next to the LLM routes, behind the
//! same run token. The protocol is MCP's streamable HTTP ([`MCP_PROTOCOL_VERSION`]): one JSON-RPC
//! message per `POST`. `initialize`, `ping` and `tools/list` answer with JSON. `tools/call` answers
//! with an event stream when the client accepts one, and sends a keepalive comment every 15 s while
//! a long call (`join`, `ask_human`, a spawn that waits) waits, so the connection through the
//! bridge (idle timeout 300 s) stays up. A JSON-only client gets a plain JSON body when the call
//! answers within 15 s, else a space every 15 s and then the result (leading whitespace is valid
//! JSON).
//!
//! * **Who acts**: the run is the one the token resolves to, never one named in the request.
//! * **What it may call**: [`ToolHost::tools`] lists the tools the run's capability allows.
//!   `tools/list` shows only those; calling another is a tool error "not permitted", and the host
//!   records the denial.
//! * **Calls survive a dropped connection**: each call runs in its own task, keyed by the run, the
//!   tool and the JSON-RPC id. Calling the same tool again with the same id waits for the same
//!   result, so a client that reconnects gets it without a second spawn or a second wait. Results
//!   are kept for at least an hour, and dropped by the first call that comes after that.
//!   `kestrel.messages` is not kept: it answers at once, and a reconnected client that starts its
//!   ids again would get old messages. Its retry is safe with `after` instead.
//! * **Limits**, so a sandbox cannot make the gateway hold much: bodies up to 1 MiB (413), ids a
//!   string of at most 128 bytes or an integer, per run a burst of 20 tool calls refilled at 10 per
//!   second, at most 8 calls waiting and 256 results kept (the oldest finished go first), and at
//!   most 256 children and a day's timeout per join. An unknown token is 401 and a finished run 410; neither echoes it.
//! * **Long calls held open, across all runs**: a `join`, a question or a spawn that waits holds
//!   its connection (through the shared bridge) until it answers, so at most
//!   [`crate::GatewayLimits::waiting_calls`] are open at once. A call past that gets a tool error
//!   at once and the host records it ([`ToolHost::refused`]): a new call starts nothing, and one
//!   called again with the id of a call still running is told so. A finished call is answered
//!   again whatever the count.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use kestrel_types::{MicroUsd, RunId};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::AppState;

/// The MCP protocol version this server speaks (streamable HTTP).
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// Versions a client may ask for in `initialize`; the tools work the same in each.
const SUPPORTED_VERSIONS: [&str; 2] = [MCP_PROTOCOL_VERSION, "2025-03-26"];

/// Largest MCP request body.
const MAX_BYTES: usize = 1024 * 1024;
/// How often a waiting call sends a keepalive comment.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// How long a finished call's result is kept for a client that calls again.
const KEEP_RESULT: Duration = Duration::from_secs(60 * 60);
/// Tool calls a run may make at once, and how many come back per second.
const BURST: f64 = 20.0;
const REFILL_PER_SECOND: f64 = 10.0;
/// Calls of one run that may wait at once, and results of one run kept.
const MAX_WAITING: usize = 8;
/// Long tool calls held open at once across all runs when [`crate::GatewayLimits`] is not set:
/// the engine's default tree limit, so the waiting parents of one tree can never fill it.
pub const DEFAULT_WAITING_CALLS: usize = 256;
const MAX_KEPT: usize = 256;
/// Longest string id, and most children in one join.
const MAX_ID_BYTES: usize = 128;
const MAX_CHILDREN: usize = 256;
/// Runs whose call allowance is remembered before full ones are forgotten.
const MAX_BUCKETS: usize = 1024;
/// How often expired results are looked for.
const PRUNE_EVERY: Duration = Duration::from_secs(60);
/// Longest a join may wait.
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// A Kestrel tool a sandbox can call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KestrelTool {
    Spawn,
    Join,
    AskHuman,
    /// `kestrel.messages`: the messages a person sent the run, each handed over once (#95).
    Messages,
}

impl KestrelTool {
    pub const ALL: [KestrelTool; 4] = [
        KestrelTool::Spawn,
        KestrelTool::Join,
        KestrelTool::AskHuman,
        KestrelTool::Messages,
    ];

    /// Its MCP name, which is also its name in a capability's tools.
    pub fn name(self) -> &'static str {
        match self {
            KestrelTool::Spawn => "kestrel.spawn",
            KestrelTool::Join => "kestrel.join",
            KestrelTool::AskHuman => "kestrel.ask_human",
            KestrelTool::Messages => "kestrel.messages",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    fn describe(self) -> Value {
        let (description, schema) = match self {
            KestrelTool::Spawn => (
                "Start a child run of another agent with its own budget, carved out of yours. \
                 Returns the child's run id; with wait, also its result. When the child agent's \
                 spec asks for more spawn depth than you can give, it gets what you can give, and \
                 the answer says so in depth and note.",
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
            KestrelTool::Messages => (
                "Get the messages a person sent you, oldest first, each with its seq. Check \
                 between steps and before you finish, and take what they say into account. Pass \
                 after, the seq of the last message you got (0 at first), to get every later \
                 one: a call you repeat with the same after loses nothing. Without after, you \
                 get each message once.",
                json!({
                    "type": "object",
                    "properties": {
                        "after": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "The seq of the last message you got; 0 for none yet."
                        }
                    }
                }),
            ),
        };
        json!({ "name": self.name(), "description": description, "inputSchema": schema })
    }
}

/// Why a host cannot say which tools a run may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolsError {
    /// The run has finished: it can call nothing more (410).
    Gone,
    /// The host cannot tell right now, for example its store failed (503). Nothing is denied.
    Unavailable,
}

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
    /// The tools `run` may call.
    async fn tools(&self, run: RunId) -> Result<Vec<KestrelTool>, ToolsError>;
    async fn spawn(&self, run: RunId, call: SpawnCall) -> Result<Value, String>;
    async fn join(&self, run: RunId, call: JoinCall) -> Result<Value, String>;
    /// Asks a person; the answer text once they decide.
    async fn ask_human(&self, run: RunId, question: String) -> Result<String, String>;
    /// The messages a person sent `run`, now delivered: those it has not taken yet, or with
    /// `after` every one after that `seq`, taken or not. The default has none to give.
    async fn messages(&self, _run: RunId, _after: Option<u64>) -> Result<Value, String> {
        Err("this host keeps no messages for agents".into())
    }
    /// Records that `run`'s call of `tool` was refused for lack of room, with the reason the
    /// agent was given. Does nothing unless the host records it.
    async fn refused(&self, _run: RunId, _tool: &str, _reason: &str) {}
    /// Records that `run` called `tool`, which it may not.
    async fn denied(&self, run: RunId, tool: &str);
}

/// A host with no tools, for a gateway that serves only the LLM routes.
pub struct NoTools;

#[async_trait]
impl ToolHost for NoTools {
    async fn tools(&self, _: RunId) -> Result<Vec<KestrelTool>, ToolsError> {
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

/// A call's result and when it came, once it is done, shared by every request with the same run
/// and id. Only the channel holds it: no task stays behind per call.
type CallResult = watch::Receiver<Option<(Value, Instant)>>;

/// One run's calls: in flight or recently finished, by id, each with its order of arrival.
#[derive(Default)]
struct RunCalls {
    by_id: HashMap<String, (CallResult, u64)>,
    next: u64,
}

/// Calls in flight or recently finished, by run, and each run's call allowance.
pub(crate) struct McpState {
    calls: Mutex<HashMap<RunId, RunCalls>>,
    buckets: Mutex<HashMap<RunId, (f64, Instant)>>,
    /// When expired results were last dropped.
    pruned: Mutex<Option<Instant>>,
    /// Tool calls whose answer is still open, across all runs, and the most there may be.
    held: Arc<AtomicUsize>,
    max_held: usize,
}

/// Why a call could not be started or waited on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoRoom {
    /// The run already has [`MAX_WAITING`] calls waiting.
    Busy,
    /// No room for one more call held open, and the call is new: nothing was started.
    FullNew,
    /// No room for one more call held open, and the call is already running.
    FullRunning,
}

/// Room for one tool call held open; given back when dropped, which is when its answer has been
/// sent or its client went away.
struct Held(Arc<AtomicUsize>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl McpState {
    /// No calls yet; at most `max_held` long calls held open at once (at least 1).
    pub(crate) fn new(max_held: usize) -> McpState {
        McpState {
            calls: Mutex::default(),
            buckets: Mutex::default(),
            pruned: Mutex::default(),
            held: Arc::default(),
            max_held: max_held.max(1),
        }
    }

    /// Room for one more call held open, or `None` when [`McpState::max_held`] are.
    fn hold(&self) -> Option<Held> {
        let mut now = self.held.load(Ordering::SeqCst);
        loop {
            if now >= self.max_held {
                return None;
            }
            match self
                .held
                .compare_exchange(now, now + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(Held(self.held.clone())),
                Err(actual) => now = actual,
            }
        }
    }

    /// Takes one tool call from `run`'s allowance; `false` when it has none left. The Travsr
    /// route draws on the same allowance.
    pub(crate) fn allow(&self, run: RunId) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(&run) {
            // A run whose allowance has refilled is as good as new: forget it.
            buckets.retain(|_, (tokens, at)| {
                *tokens + now.duration_since(*at).as_secs_f64() * REFILL_PER_SECOND < BURST
            });
        }
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
pub(crate) fn rpc_error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    let body = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    (status, Json(body)).into_response()
}

pub(crate) fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// The `initialize` result of a server named `name`: the client's protocol version when this
/// server speaks it, else this server's own.
pub(crate) fn initialized(name: &str, params: &Value) -> Value {
    json!({
        "protocolVersion": params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .filter(|v| SUPPORTED_VERSIONS.contains(v))
            .unwrap_or(MCP_PROTOCOL_VERSION),
        "capabilities": { "tools": {} },
        "serverInfo": { "name": name, "version": env!("CARGO_PKG_VERSION") }
    })
}

/// The run the token names and the request body, read once the caller is known. `Err` is the
/// answer: 401 for an unknown token, 410 for a run that has ended (503 when that cannot be told), 413 for a body over
/// [`MAX_BYTES`], 400 when it cannot be read.
pub(crate) async fn caller(
    state: &AppState,
    token: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<(RunId, axum::body::Bytes), Box<Response>> {
    let Some(run) = state.deps.resolver.resolve(token).await else {
        return Err(Box::new(rpc_error(
            StatusCode::UNAUTHORIZED,
            Value::Null,
            -32001,
            "unknown run token",
        )));
    };
    if let Some((status, why)) = crate::refused(state, run).await {
        let code = if status == StatusCode::GONE {
            -32003
        } else {
            -32603
        };
        return Err(Box::new(rpc_error(status, Value::Null, code, why)));
    }
    let too_large = || {
        rpc_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            Value::Null,
            -32002,
            "request body is larger than 1 MiB",
        )
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_BYTES) {
        return Err(Box::new(too_large()));
    }
    match axum::body::to_bytes(body, MAX_BYTES).await {
        Ok(bytes) => Ok((run, bytes)),
        Err(e) if e.to_string().contains("length limit") => Err(Box::new(too_large())),
        Err(_) => Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32700,
            "the request body could not be read",
        ))),
    }
}

/// The answer for a run whose tools the host cannot list: 410 once it has finished, else 503.
pub(crate) fn tools_error(e: ToolsError) -> Response {
    match e {
        ToolsError::Gone => rpc_error(StatusCode::GONE, Value::Null, -32003, crate::RUN_ENDED),
        ToolsError::Unavailable => rpc_error(
            StatusCode::SERVICE_UNAVAILABLE,
            Value::Null,
            -32603,
            "the tools are unavailable right now, try again",
        ),
    }
}

/// One JSON-RPC request to answer.
pub(crate) struct Request {
    pub(crate) id: Value,
    pub(crate) method: String,
    pub(crate) params: Value,
}

/// The request in `bytes`. `Err` is the answer: 400 for a message that is not a JSON-RPC
/// request, 202 for a notification or a client's answer (nothing to answer).
pub(crate) fn request(bytes: &[u8]) -> Result<Request, Box<Response>> {
    let Ok(message) = serde_json::from_slice::<Value>(bytes) else {
        return Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32700,
            "parse error",
        )));
    };
    let Some(object) = message.as_object() else {
        return Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32600,
            "invalid request: send one JSON-RPC object",
        )));
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32600,
            "invalid request: jsonrpc must be \"2.0\"",
        )));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A client's answer to a server request (this server sends none): nothing to do.
        if object.contains_key("result") || object.contains_key("error") {
            return Err(Box::new(StatusCode::ACCEPTED.into_response()));
        }
        return Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32600,
            "invalid request: no method",
        )));
    };
    if id.is_null() {
        // A notification, such as `notifications/initialized`: nothing to answer.
        return Err(Box::new(StatusCode::ACCEPTED.into_response()));
    }
    // The id is kept with a call's result, so it must be small.
    let short_text = id.as_str().is_some_and(|s| s.len() <= MAX_ID_BYTES);
    if !short_text && !id.is_i64() && !id.is_u64() {
        return Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32600,
            "invalid request: id must be a string of at most 128 bytes or an integer",
        )));
    }
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    Ok(Request {
        id,
        method: method.to_string(),
        params,
    })
}

/// A tool result for the model: its text, structured content for JSON, and whether it failed.
pub(crate) fn tool_result(outcome: Result<Value, String>) -> Value {
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
    let (run, bytes) = match caller(&state, &token, &headers, body).await {
        Ok(read) => read,
        Err(answer) => return *answer,
    };
    let tools = match state.deps.tools.tools(run).await {
        Ok(tools) => tools,
        Err(e) => return tools_error(e),
    };
    let Request { id, method, params } = match request(&bytes) {
        Ok(request) => request,
        Err(answer) => return *answer,
    };
    match method.as_str() {
        "initialize" => Json(rpc_result(id, initialized("kestrel", &params))).into_response(),
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
    /// With `after`, the seq of the last message the agent got.
    Messages(Option<u64>),
}

impl Task {
    /// A call that may wait a long time for its answer: a join, a question, a spawn that waits.
    /// A spawn without `wait` answers at once.
    fn is_long(&self) -> bool {
        match self {
            Task::Spawn(call) => call.wait,
            Task::Join(_) | Task::Ask(_) => true,
            Task::Messages(_) => false,
        }
    }
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
    if matches!(task, Task::Messages(_)) {
        // Answers at once and is never kept by id: a client that reconnects starts its ids again
        // and would get an old answer. A retry passes `after` instead.
        let value = perform(state.deps.tools.clone(), run, task).await;
        return answer(id, value, events);
    }
    let (mut result, held) = match start(state, run, tool, &id, task) {
        Ok(started) => started,
        Err(NoRoom::Busy) => {
            return rpc_error(
                StatusCode::TOO_MANY_REQUESTS,
                id,
                -32000,
                "too many tool calls waiting for this run, wait for one to finish",
            );
        }
        Err(full) => {
            let what = if full == NoRoom::FullRunning {
                "this call is still running; call again later with the same id for its result"
            } else {
                "nothing was started; call again later"
            };
            let reason = format!(
                "too many tool calls are waiting across all runs (the limit is {}, \
                 gateway.max_waiting_calls): {what}",
                state.mcp.max_held
            );
            state.deps.tools.refused(run, tool.name(), &reason).await;
            return Json(rpc_result(id, tool_result(Err(reason)))).into_response();
        }
    };
    let wait = async move {
        // Given back once the answer is ready, or when the client goes away first.
        let _held = held;
        loop {
            if let Some((value, _)) = result.borrow_and_update().clone() {
                return value;
            }
            if result.changed().await.is_err() {
                return tool_result(Err("the call failed".into()));
            }
        }
    };
    if !events {
        // A call that answers within one keepalive period gets a plain JSON body. A longer one
        // gets a space every period, so the bridge never sees it idle, then the result.
        let mut wait = Box::pin(wait);
        return match tokio::time::timeout(KEEPALIVE, &mut wait).await {
            Ok(value) => Json(rpc_result(id, value)).into_response(),
            Err(_) => padded(id, wait),
        };
    }
    let stream = futures::stream::once(async move {
        let message = rpc_result(id, wait.await);
        Ok::<_, Infallible>(Event::default().event("message").data(message.to_string()))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE))
        .into_response()
}

/// The answer to a call that is done: one event when the client accepts an event stream, else
/// plain JSON.
fn answer(id: Value, value: Value, events: bool) -> Response {
    let message = rpc_result(id, value);
    if !events {
        return Json(message).into_response();
    }
    let event = Event::default().event("message").data(message.to_string());
    Sse::new(futures::stream::once(
        async move { Ok::<_, Infallible>(event) },
    ))
    .into_response()
}

/// Runs `task` for `run` on `host` in a task of its own: a client that goes away does not cancel
/// it, and a host call that panics still ends with a result, so it never counts as waiting
/// forever.
async fn perform(host: Arc<dyn ToolHost>, run: RunId, task: Task) -> Value {
    let work = tokio::spawn(async move {
        match task {
            Task::Spawn(call) => host.spawn(run, call).await,
            Task::Join(call) => host.join(run, call).await,
            Task::Ask(question) => host.ask_human(run, question).await.map(Value::String),
            Task::Messages(after) => host.messages(run, after).await,
        }
    });
    let outcome = work
        .await
        .unwrap_or_else(|_| Err("the call failed inside Kestrel; try again".into()));
    tool_result(outcome)
}

/// A JSON answer for a client that does not accept an event stream, to a call still waiting: a
/// space at once and every [`KEEPALIVE`] after, then the JSON-RPC result. Leading whitespace is
/// valid JSON, so the body still parses as the result.
fn padded<F>(id: Value, wait: Pin<Box<F>>) -> Response
where
    F: Future<Output = Value> + Send + 'static,
{
    let mut ticks = tokio::time::interval(KEEPALIVE);
    // A client that stopped reading for a while gets one space when it reads again, not a burst.
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let stream = futures::stream::unfold(Some((wait, ticks, id)), |open| async move {
        let (mut wait, mut ticks, id) = open?;
        tokio::select! {
            biased;
            value = &mut wait => Some((Bytes::from(rpc_result(id, value).to_string()), None)),
            _ = ticks.tick() => Some((Bytes::from_static(b" "), Some((wait, ticks, id)))),
        }
    })
    .map(Ok::<_, Infallible>);
    (
        [(header::CONTENT_TYPE, "application/json")],
        Body::from_stream(stream),
    )
        .into_response()
}

/// The call for `(run, id)` and the room it holds while its answer is open: the one already done
/// (no room needed), the one still running, or a new one in its own task, so a client that goes
/// away does not cancel it. A long call holds its connection (through the shared bridge) until it
/// answers, maybe for hours, so those are capped across all runs ([`McpState::hold`]). A running
/// call needs room whatever the new request asks for, since the client waits on it; a new call
/// needs room only when it is long. Decided under the lock, so no result can go between looking
/// and starting. At [`MAX_KEPT`] results, the oldest finished one is let go.
fn start(
    state: &Arc<AppState>,
    run: RunId,
    tool: KestrelTool,
    id: &Value,
    task: Task,
) -> Result<(CallResult, Option<Held>), NoRoom> {
    // By tool and id: a reused id that names another tool is another call.
    let key = format!("{} {id}", tool.name());
    let mut calls = state
        .mcp
        .calls
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    // Results older than an hour go, for every run, so ended runs leave nothing behind. At most
    // once a minute: it looks at every kept result.
    let now = Instant::now();
    let mut pruned = state
        .mcp
        .pruned
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if pruned.is_none_or(|at| now.duration_since(at) >= PRUNE_EVERY) {
        *pruned = Some(now);
        calls.retain(|_, runs| {
            runs.by_id.retain(|_, (r, _)| {
                r.borrow()
                    .as_ref()
                    .is_none_or(|(_, at)| now.duration_since(*at) < KEEP_RESULT)
            });
            !runs.by_id.is_empty()
        });
    }
    drop(pruned);
    let mine = calls.entry(run).or_default();
    if let Some((existing, _)) = mine.by_id.get(&key) {
        if existing.borrow().is_some() {
            return Ok((existing.clone(), None));
        }
        let held = state.mcp.hold().ok_or(NoRoom::FullRunning)?;
        return Ok((existing.clone(), Some(held)));
    }
    let waiting = mine
        .by_id
        .values()
        .filter(|(r, _)| r.borrow().is_none())
        .count();
    if waiting >= MAX_WAITING {
        return Err(NoRoom::Busy);
    }
    let held = if task.is_long() {
        Some(state.mcp.hold().ok_or(NoRoom::FullNew)?)
    } else {
        None
    };
    if mine.by_id.len() >= MAX_KEPT {
        let oldest = mine
            .by_id
            .iter()
            .filter(|(_, (r, _))| r.borrow().is_some())
            .min_by_key(|(_, (_, order))| *order)
            .map(|(k, _)| k.clone());
        if let Some(oldest) = oldest {
            mine.by_id.remove(&oldest);
        }
    }
    let (tx, rx) = watch::channel(None);
    let order = mine.next;
    mine.next += 1;
    mine.by_id.insert(key, (rx.clone(), order));
    drop(calls);
    let host = state.deps.tools.clone();
    tokio::spawn(async move {
        let value = perform(host, run, task).await;
        let _ = tx.send(Some((value, Instant::now())));
    });
    Ok((rx, held))
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
                if list.len() > MAX_CHILDREN {
                    return Err("children may name at most 256 runs".into());
                }
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
                    let wait = t
                        .as_f64()
                        .and_then(|s| Duration::try_from_secs_f64(s).ok())
                        .filter(|d| *d <= MAX_TIMEOUT)
                        .ok_or("timeout_s must be a number of seconds from 0 to 86400")?;
                    Some(wait)
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
        KestrelTool::Messages => match args.get("after") {
            None | Some(Value::Null) => Ok(Task::Messages(None)),
            Some(after) => after
                .as_u64()
                .map(|seq| Task::Messages(Some(seq)))
                .ok_or_else(|| {
                    "after must be the seq of the last message you got, a whole number from 0"
                        .to_string()
                }),
        },
    }
}

fn dollars(text: &str) -> Result<MicroUsd, String> {
    MicroUsd::parse_decimal(text)
        .ok_or_else(|| "budget_usd must be dollars, such as \"0.50\"".to_string())
}
