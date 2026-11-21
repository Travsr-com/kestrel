//! Travsr's code graph tools for agents in a sandbox, served over MCP at `/r/{run_token}/travsr`.
//!
//! The gateway is a sandbox's only way out, so Travsr is reached through it, behind the run
//! token, like the Kestrel tools at `/r/{run_token}/mcp` (same JSON-RPC handling, same limits).
//! The route exists only when the gateway has a [`TravsrHost`]; without one it is 404.
//!
//! * **Who asks**: the run is the one the token resolves to. Its calls go to its own Travsr
//!   server, which is bound to its own clone, so a request can never name another repo.
//! * **What it may call**: the read tools in [`TRAVSR_TOOLS`] that the run's capability grants
//!   (`travsr.<tool>`), listed by Travsr's own names (`get_callers`). Anything else is a tool
//!   error "not permitted" and nothing is forwarded. The host records the denial of a read tool
//!   the run was not granted; a name off the allowlist is neither recorded nor echoed.
//! * **A call lives as long as its request**: it is stopped when it times out and when the
//!   request is dropped.
//! * **Arguments**: passed on as they are, without `repo`, which is dropped from calls and
//!   from the listed schemas.
//! * **Failure is the agent's to handle, never the run's**: a host error or a call slower than
//!   [`TravsrHost::call_timeout`] is a tool error with a short reason.
//! * **Answers** are cut at [`TRAVSR_RESPONSE_CAP`] bytes of text, with a marker saying so.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use kestrel_types::RunId;
use serde_json::{Value, json};

use crate::AppState;
use crate::mcp::{
    Request, ToolsError, caller, initialized, request, rpc_error, rpc_result, tool_result,
    tools_error,
};

/// The Travsr tools the route serves: the read tools. Index, admin and log tools are never served.
pub const TRAVSR_TOOLS: [&str; 10] = [
    "search_symbol",
    "get_callers",
    "get_dependencies",
    "get_blast_radius",
    "get_execution_path",
    "get_repo_map",
    "get_context",
    "find_references",
    "find_pattern",
    "get_snippets",
];

/// Most bytes of text an answer keeps; the rest is cut with a marker.
pub const TRAVSR_RESPONSE_CAP: usize = 256 * 1024;

/// Longest reason a failed call gives the agent, in bytes.
const MAX_REASON: usize = 300;

/// Reaches Travsr for the run the token resolved to. A host never takes a repo from a request:
/// it knows each run's clone itself.
#[async_trait]
pub trait TravsrHost: Send + Sync {
    /// The Travsr tools `run` may call, by Travsr's names (`get_callers`): its capability's
    /// `travsr.<tool>` entries. None while Travsr is unavailable for the run. The route also
    /// keeps only the tools in [`TRAVSR_TOOLS`].
    async fn tools(&self, run: RunId) -> Result<Vec<String>, ToolsError>;
    /// Travsr's own `tools/list` entries for `run` (name, description, input schema).
    async fn schemas(&self, run: RunId) -> Result<Vec<Value>, String>;
    /// Calls `tool` with `args` on `run`'s Travsr. `Ok` is Travsr's tool result (its `content`
    /// and `isError`); `Err` is why Travsr could not answer.
    async fn call(&self, run: RunId, tool: &str, args: Value) -> Result<Value, String>;
    /// Records that `run` called `tool` (as `travsr.<tool>`), a read tool it was not granted.
    /// Never called with a name off [`TRAVSR_TOOLS`].
    async fn denied(&self, run: RunId, tool: &str);
    /// The longest one call, or one listing, may take.
    fn call_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }
}

/// `POST /r/{run_token}/travsr`.
pub(crate) async fn post(
    state: Arc<AppState>,
    host: Arc<dyn TravsrHost>,
    token: String,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let (run, bytes) = match caller(&state, &token, &headers, body).await {
        Ok(read) => read,
        Err(answer) => return *answer,
    };
    let granted = match host.tools(run).await {
        Ok(tools) => tools,
        Err(e) => return tools_error(e),
    };
    // Only the read tools, whatever the capability says.
    let tools: Vec<String> = granted
        .into_iter()
        .filter(|t| TRAVSR_TOOLS.contains(&t.as_str()))
        .collect();
    let Request { id, method, params } = match request(&bytes) {
        Ok(request) => request,
        Err(answer) => return *answer,
    };
    match method.as_str() {
        "initialize" => Json(rpc_result(id, initialized("travsr", &params))).into_response(),
        "ping" => Json(rpc_result(id, json!({}))).into_response(),
        "tools/list" => list(host, run, &tools, id).await,
        "tools/call" => call(&state, host, run, &tools, id, &params).await,
        _ => rpc_error(StatusCode::OK, id, -32601, "method not found"),
    }
}

/// `GET /r/{run_token}/travsr`: no stream is offered, as on `/mcp`.
pub(crate) async fn get() -> Response {
    crate::mcp::get().await
}

/// `tools/list`: Travsr's schemas of the tools the run may call, without `repo`.
async fn list(host: Arc<dyn TravsrHost>, run: RunId, tools: &[String], id: Value) -> Response {
    if tools.is_empty() {
        return Json(rpc_result(id, json!({ "tools": [] }))).into_response();
    }
    let limit = host.call_timeout();
    let asked = in_task(limit, async move { host.schemas(run).await }).await;
    match asked {
        Ok(schemas) => {
            let listed: Vec<Value> = schemas
                .into_iter()
                .filter(|s| {
                    s.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| tools.iter().any(|t| t == n))
                })
                .map(without_repo)
                .collect();
            Json(rpc_result(id, json!({ "tools": listed }))).into_response()
        }
        Err(reason) => rpc_error(StatusCode::OK, id, -32603, &reason),
    }
}

/// `schema` with `repo` gone from its input properties and required list.
fn without_repo(mut schema: Value) -> Value {
    if let Some(input) = schema.get_mut("inputSchema") {
        if let Some(props) = input.get_mut("properties").and_then(Value::as_object_mut) {
            props.remove("repo");
        }
        if let Some(required) = input.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|r| r != "repo");
        }
    }
    schema
}

async fn call(
    state: &Arc<AppState>,
    host: Arc<dyn TravsrHost>,
    run: RunId,
    tools: &[String],
    id: Value,
    params: &Value,
) -> Response {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return rpc_error(
            StatusCode::OK,
            id,
            -32602,
            "invalid params: name is required",
        );
    };
    if !state.mcp.allow(run) {
        return rpc_error(
            StatusCode::TOO_MANY_REQUESTS,
            id,
            -32000,
            "too many tool calls, slow down",
        );
    }
    // A name off the allowlist is the agent's own text, of any length: it is neither recorded
    // nor echoed, so it cannot flood the run's log or the answer.
    if !TRAVSR_TOOLS.contains(&name) {
        let reason = format!(
            "not permitted: Kestrel serves only these Travsr read tools: {}",
            TRAVSR_TOOLS.join(", ")
        );
        return Json(rpc_result(id, tool_result(Err(reason)))).into_response();
    }
    let full = format!("travsr.{name}");
    if !tools.iter().any(|t| t == name) {
        host.denied(run, &full).await;
        let reason = format!("not permitted: this run's capabilities do not include {full}");
        return Json(rpc_result(id, tool_result(Err(reason)))).into_response();
    }
    let mut args = match params.get("arguments") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(args)) => args.clone(),
        Some(_) => {
            return rpc_error(
                StatusCode::OK,
                id,
                -32602,
                "invalid params: arguments must be an object",
            );
        }
    };
    // The run's server is bound to its clone: no call names a repo or a host path.
    args.remove("repo");
    let limit = host.call_timeout();
    let tool = name.to_string();
    let answer = in_task(limit, async move {
        host.call(run, &tool, Value::Object(args)).await
    })
    .await;
    Json(rpc_result(id, shaped(answer))).into_response()
}

/// Aborts its task when dropped: when the call times out, and when the request handling it is
/// dropped (the client hung up), so a call never outlives its request and holds nothing after.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs `work` in its own task, so a host that panics still gives an answer, and gives up after
/// `limit`. Every `Err` is a reason for the agent, starting with "Travsr".
async fn in_task<T: Send + 'static>(
    limit: Duration,
    work: impl Future<Output = Result<T, String>> + Send + 'static,
) -> Result<T, String> {
    let mut task = AbortOnDrop(tokio::spawn(work));
    match tokio::time::timeout(limit, &mut task.0).await {
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(reason))) => Err(format!("Travsr could not answer: {}", short(&reason))),
        Ok(Err(_)) => Err("Travsr: the call failed inside Kestrel; try again".into()),
        Err(_) => Err(format!(
            "Travsr did not answer within {} s; try again or a narrower query",
            limit.as_secs()
        )),
    }
}

/// The agent's tool result for Travsr's answer: its text, cut at [`TRAVSR_RESPONSE_CAP`].
fn shaped(answer: Result<Value, String>) -> Value {
    let result = match answer {
        Ok(result) => result,
        Err(reason) => return tool_result(Err(reason)),
    };
    let failed = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = match result.get("content").and_then(Value::as_array) {
        Some(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        None => result.to_string(),
    };
    json!({ "content": [{ "type": "text", "text": capped(text) }], "isError": failed })
}

/// `text` cut to at most [`TRAVSR_RESPONSE_CAP`] bytes (on a character boundary), with a marker.
fn capped(mut text: String) -> String {
    let size = text.len();
    if size <= TRAVSR_RESPONSE_CAP {
        return text;
    }
    text.truncate(boundary(&text, TRAVSR_RESPONSE_CAP));
    text.push_str(&format!(
        "\n[truncated by Kestrel: Travsr's answer was {size} bytes, only the first {TRAVSR_RESPONSE_CAP} are shown; ask a narrower query]"
    ));
    text
}

/// The last character boundary of `text` at or before `at`.
fn boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// `reason` cut to [`MAX_REASON`] bytes.
fn short(reason: &str) -> String {
    let reason = reason.trim();
    if reason.len() <= MAX_REASON {
        return reason.to_string();
    }
    format!("{}...", &reason[..boundary(reason, MAX_REASON)])
}
