//! Issue #44: Travsr's read tools served to sandboxes over MCP at `/r/{token}/travsr`, against a
//! fake `TravsrHost`. Requests go through the router in process.
//!
//! Test names start with the scenario number of the issue they cover (`s3_...`); rules the route
//! adds on top have no number.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{FakeBudget, FakeSink, config};
use kestrel_gateway::{
    GatewayDeps, KestrelTool, NoTools, RunResolver, TRAVSR_RESPONSE_CAP, TRAVSR_TOOLS, ToolsError,
    TravsrHost, router,
};
use kestrel_types::RunId;
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN_A: &str = "rt_live_travsr_a";
const TOKEN_B: &str = "rt_live_travsr_b";
const FINISHED: &str = "rt_live_travsr_done";
const BROKEN: &str = "rt_live_travsr_broken";

struct Tokens(HashMap<String, RunId>);

#[async_trait]
impl RunResolver for Tokens {
    async fn resolve(&self, token: &str) -> Option<RunId> {
        self.0.get(token).copied()
    }
}

/// What the fake answers a call with.
#[derive(Clone)]
enum Answer {
    /// Travsr's result: one text item holding the run, the tool and the arguments it got.
    Echo,
    /// A result whose text is this many bytes.
    Big(usize),
    /// Travsr's own error result (`isError: true`).
    ToolError(String),
    /// The host could not reach Travsr.
    Fail(String),
    /// Never answers.
    Hang,
    /// The host falls over.
    Panic,
    /// A result with no content array.
    Bare,
}

/// A fake host: which Travsr tools each run may call and every call it got.
struct FakeTravsr {
    allowed: Mutex<HashMap<RunId, Vec<String>>>,
    gone: Mutex<Vec<RunId>>,
    unavailable: Mutex<Vec<RunId>>,
    answer: Mutex<Answer>,
    schemas: Mutex<Result<Vec<Value>, String>>,
    calls: Mutex<Vec<(RunId, String, Value)>>,
    denied: Mutex<Vec<(RunId, String)>>,
    /// Set when work the host was hanging in is dropped.
    hung_dropped: Arc<AtomicBool>,
    timeout: Duration,
}

/// Sets its flag when dropped.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl FakeTravsr {
    fn new(timeout: Duration) -> Self {
        let schemas = TRAVSR_TOOLS
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "description": format!("Travsr's {name}"),
                    "inputSchema": {
                        "type": "object",
                        "properties": { "symbol": { "type": "string" }, "repo": { "type": "string" } },
                        "required": ["symbol", "repo"],
                        "additionalProperties": false
                    }
                })
            })
            // A tool Travsr has that the route never serves.
            .chain([json!({ "name": "index_repo", "inputSchema": { "type": "object" } })])
            .collect();
        Self {
            allowed: Mutex::default(),
            gone: Mutex::default(),
            unavailable: Mutex::default(),
            answer: Mutex::new(Answer::Echo),
            schemas: Mutex::new(Ok(schemas)),
            calls: Mutex::default(),
            denied: Mutex::default(),
            hung_dropped: Arc::default(),
            timeout,
        }
    }

    fn calls(&self) -> Vec<(RunId, String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn set(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    /// Never returns; notes it when dropped.
    async fn hang(&self) {
        let _dropped = Dropped(self.hung_dropped.clone());
        std::future::pending::<()>().await;
    }
}

#[async_trait]
impl TravsrHost for FakeTravsr {
    async fn tools(&self, run: RunId) -> Result<Vec<String>, ToolsError> {
        if self.gone.lock().unwrap().contains(&run) {
            return Err(ToolsError::Gone);
        }
        if self.unavailable.lock().unwrap().contains(&run) {
            return Err(ToolsError::Unavailable);
        }
        Ok(self
            .allowed
            .lock()
            .unwrap()
            .get(&run)
            .cloned()
            .unwrap_or_default())
    }

    async fn schemas(&self, _run: RunId) -> Result<Vec<Value>, String> {
        let hangs = matches!(*self.answer.lock().unwrap(), Answer::Hang);
        if hangs {
            self.hang().await;
        }
        self.schemas.lock().unwrap().clone()
    }

    async fn call(&self, run: RunId, tool: &str, args: Value) -> Result<Value, String> {
        self.calls
            .lock()
            .unwrap()
            .push((run, tool.to_string(), args.clone()));
        let answer = self.answer.lock().unwrap().clone();
        match answer {
            Answer::Echo => {
                let text = json!({ "run": run.to_string(), "tool": tool, "args": args });
                Ok(json!({ "content": [{ "type": "text", "text": text.to_string() }] }))
            }
            Answer::Big(n) => {
                Ok(json!({ "content": [{ "type": "text", "text": "é".repeat(n / 2) }] }))
            }
            Answer::ToolError(text) => {
                Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": true }))
            }
            Answer::Fail(reason) => Err(reason),
            Answer::Hang => {
                self.hang().await;
                Err("hung".into())
            }
            Answer::Panic => panic!("the host fell over"),
            Answer::Bare => Ok(json!({ "answer": 42 })),
        }
    }

    async fn denied(&self, run: RunId, tool: &str) {
        self.denied.lock().unwrap().push((run, tool.to_string()));
    }

    fn call_timeout(&self) -> Duration {
        self.timeout
    }
}

struct Setup {
    app: Router,
    host: Arc<FakeTravsr>,
    a: RunId,
    b: RunId,
}

fn setup_with(allowed_a: &[&str], timeout: Duration) -> Setup {
    let (a, b, done, broken) = (RunId::new(), RunId::new(), RunId::new(), RunId::new());
    let tokens = Tokens(HashMap::from([
        (TOKEN_A.to_string(), a),
        (TOKEN_B.to_string(), b),
        (FINISHED.to_string(), done),
        (BROKEN.to_string(), broken),
    ]));
    let host = Arc::new(FakeTravsr::new(timeout));
    let names = |list: &[&str]| list.iter().map(|t| (*t).to_string()).collect::<Vec<_>>();
    host.allowed.lock().unwrap().insert(a, names(allowed_a));
    host.allowed.lock().unwrap().insert(b, names(&TRAVSR_TOOLS));
    host.gone.lock().unwrap().push(done);
    host.unavailable.lock().unwrap().push(broken);
    let deps = GatewayDeps {
        resolver: Arc::new(tokens),
        budget: Arc::new(FakeBudget::new(true)),
        meter: Arc::new(FakeSink::default()),
        tools: Arc::new(NoTools),
        travsr: Some(host.clone()),
    };
    let app = router(config("http://127.0.0.1:9", "http://127.0.0.1:9"), deps);
    Setup { app, host, a, b }
}

fn setup(allowed_a: &[&str]) -> Setup {
    setup_with(allowed_a, Duration::from_secs(30))
}

fn rpc(id: Value, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn call(id: u64, tool: &str, arguments: Value) -> Value {
    rpc(
        json!(id),
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )
}

async fn send_raw(app: &Router, token: &str, body: Vec<u8>) -> axum::response::Response {
    let request = Request::post(format!("/r/{token}/travsr"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(body))
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

async fn send(app: &Router, token: &str, message: &Value) -> axum::response::Response {
    send_raw(app, token, message.to_string().into_bytes()).await
}

async fn body(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Sends `message` as run A and returns the JSON-RPC answer, which must be 200.
async fn ask(s: &Setup, message: &Value) -> Value {
    let response = send(&s.app, TOKEN_A, message).await;
    assert_eq!(response.status(), StatusCode::OK);
    body(response).await
}

/// The text of a tool result.
fn text(msg: &Value) -> String {
    msg["result"]["content"][0]["text"]
        .as_str()
        .expect("text content")
        .to_string()
}

fn is_error(msg: &Value) -> bool {
    msg["result"]["isError"] == json!(true)
}

#[tokio::test]
async fn initialize_and_ping_answer_as_travsr() {
    let s = setup(&["get_callers"]);
    let init = rpc(
        json!(1),
        "initialize",
        json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "claude-code" } }),
    );
    let msg = ask(&s, &init).await;
    assert_eq!(msg["result"]["serverInfo"]["name"], "travsr");
    assert_eq!(msg["result"]["protocolVersion"], "2025-03-26");
    assert!(msg["result"]["capabilities"]["tools"].is_object());
    let msg = ask(
        &s,
        &rpc(
            json!(1),
            "initialize",
            json!({ "protocolVersion": "1999-01-01" }),
        ),
    )
    .await;
    assert_eq!(
        msg["result"]["protocolVersion"],
        kestrel_gateway::MCP_PROTOCOL_VERSION
    );
    let msg = ask(&s, &rpc(json!("p"), "ping", json!({}))).await;
    assert_eq!(msg, json!({ "jsonrpc": "2.0", "id": "p", "result": {} }));
    let msg = ask(&s, &rpc(json!(2), "resources/list", json!({}))).await;
    assert_eq!(msg["error"]["code"], -32601);
    assert!(s.host.calls().is_empty());
}

#[tokio::test]
async fn tools_list_shows_the_granted_read_tools_by_travsr_name_without_repo() {
    let s = setup(&["get_callers", "search_symbol"]);
    let msg = ask(&s, &rpc(json!(1), "tools/list", json!({}))).await;
    let tools = msg["result"]["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(names, ["get_callers", "search_symbol"]);
    for tool in tools {
        let schema = &tool["inputSchema"];
        assert!(schema["properties"]["repo"].is_null(), "{schema}");
        assert_eq!(schema["required"], json!(["symbol"]), "{schema}");
        assert_eq!(schema["properties"]["symbol"]["type"], "string");
    }
}

#[tokio::test]
async fn tools_list_never_shows_a_granted_tool_that_is_not_on_the_allowlist() {
    let s = setup(&["get_callers", "index_repo"]);
    let msg = ask(&s, &rpc(json!(1), "tools/list", json!({}))).await;
    let names: Vec<&str> = msg["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["get_callers"]);
}

#[tokio::test]
async fn tools_list_says_why_when_travsr_cannot_list() {
    let s = setup(&["get_callers"]);
    *s.host.schemas.lock().unwrap() = Err("travsr exited".into());
    let msg = ask(&s, &rpc(json!(1), "tools/list", json!({}))).await;
    assert_eq!(msg["error"]["code"], -32603);
    let message = msg["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Travsr") && message.contains("travsr exited"),
        "{message}"
    );
}

#[tokio::test]
async fn s2_a_run_with_travsr_unavailable_lists_no_tools_and_asks_nothing() {
    // The host lists no tool for a run whose clone could not be indexed.
    let s = setup(&[]);
    *s.host.schemas.lock().unwrap() = Err("must not be asked".into());
    let msg = ask(&s, &rpc(json!(1), "tools/list", json!({}))).await;
    assert_eq!(msg["result"]["tools"], json!([]));
    let msg = ask(&s, &call(2, "get_callers", json!({ "symbol": "main" }))).await;
    assert!(is_error(&msg));
    assert!(s.host.calls().is_empty());
}

#[tokio::test]
async fn a_granted_call_is_forwarded_for_the_token_s_run() {
    let s = setup(&["get_callers"]);
    let msg = ask(
        &s,
        &call(7, "get_callers", json!({ "symbol": "Engine::new" })),
    )
    .await;
    assert_eq!(msg["id"], 7);
    assert!(!is_error(&msg), "{msg}");
    let echoed: Value = serde_json::from_str(&text(&msg)).unwrap();
    assert_eq!(echoed["run"], s.a.to_string());
    assert_eq!(echoed["args"], json!({ "symbol": "Engine::new" }));
    assert_eq!(
        s.host.calls(),
        [(
            s.a,
            "get_callers".to_string(),
            json!({ "symbol": "Engine::new" })
        )]
    );
}

#[tokio::test]
async fn s3_a_repo_argument_never_reaches_the_host() {
    let s = setup(&["get_callers"]);
    let other = "/var/lib/kestrel/workspaces/someone-else";
    let msg = ask(
        &s,
        &call(1, "get_callers", json!({ "symbol": "main", "repo": other })),
    )
    .await;
    assert!(!is_error(&msg), "{msg}");
    let calls = s.host.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2, json!({ "symbol": "main" }));
    assert!(!format!("{calls:?}").contains(other));
}

#[tokio::test]
async fn s3_an_absolute_repo_path_is_dropped_too() {
    // Travsr wants an absolute path as `repo` (Travsr-com/travsr#1117). The run's server is
    // bound to its own clone, so even the run's own absolute path is not passed on.
    let s = setup(&["search_symbol"]);
    let mine = std::env::temp_dir().join("kestrel-run-clone");
    let msg = ask(
        &s,
        &call(
            1,
            "search_symbol",
            json!({ "name": "Engine", "repo": mine }),
        ),
    )
    .await;
    assert!(!is_error(&msg), "{msg}");
    assert_eq!(s.host.calls()[0].2, json!({ "name": "Engine" }));
}

#[tokio::test]
async fn s4_a_tool_off_the_allowlist_is_refused_and_nothing_is_forwarded() {
    // Granted in the capability, still refused: only the read tools are served.
    let s = setup(&["get_callers", "index_repo"]);
    let long = "x".repeat(100_000);
    for (i, tool) in [
        "index_repo",
        "repos_remove",
        "get_index_status",
        "get_daemon_logs",
        "synonym_add",
        long.as_str(),
    ]
    .into_iter()
    .enumerate()
    {
        let msg = ask(&s, &call(i as u64, tool, json!({ "path": "/" }))).await;
        assert!(is_error(&msg), "{msg}");
        // One fixed reason, which never echoes the name the agent sent.
        let reason = text(&msg);
        assert!(reason.starts_with("not permitted"), "{reason}");
        assert!(!reason.contains(tool), "{reason}");
        assert!(reason.len() < 300, "{}", reason.len());
    }
    assert!(s.host.calls().is_empty());
    // A name off the allowlist is never recorded, so the event log cannot be flooded with it.
    assert!(s.host.denied.lock().unwrap().is_empty());
}

#[tokio::test]
async fn s5_one_granted_tool_lists_one_and_refuses_the_others() {
    let s = setup(&["search_symbol"]);
    let msg = ask(&s, &rpc(json!(1), "tools/list", json!({}))).await;
    let tools = msg["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "search_symbol");
    let msg = ask(&s, &call(2, "get_callers", json!({ "symbol": "main" }))).await;
    assert!(is_error(&msg));
    let reason = text(&msg);
    assert!(
        reason.contains("not permitted") && reason.contains("travsr.get_callers"),
        "{reason}"
    );
    assert!(s.host.calls().is_empty());
    assert_eq!(
        *s.host.denied.lock().unwrap(),
        [(s.a, "travsr.get_callers".to_string())]
    );
}

#[tokio::test]
async fn s6_a_1_mb_answer_is_cut_at_the_cap_with_a_marker() {
    let s = setup(&["get_repo_map"]);
    s.host.set(Answer::Big(1_000_000));
    let msg = ask(&s, &call(1, "get_repo_map", json!({}))).await;
    assert!(!is_error(&msg));
    let text = text(&msg);
    let (kept, marker) = text
        .split_once("\n[truncated")
        .expect("a truncation marker");
    assert!(kept.len() <= TRAVSR_RESPONSE_CAP, "{}", kept.len());
    assert!(kept.len() > TRAVSR_RESPONSE_CAP - 4, "{}", kept.len());
    assert!(kept.chars().all(|c| c == 'é'));
    assert!(marker.contains("1000000 bytes"), "{marker}");
    assert!(text.len() < TRAVSR_RESPONSE_CAP + 200);
}

#[tokio::test]
async fn an_answer_under_the_cap_is_kept_whole() {
    let s = setup(&["get_repo_map"]);
    s.host.set(Answer::Big(TRAVSR_RESPONSE_CAP));
    let msg = ask(&s, &call(1, "get_repo_map", json!({}))).await;
    assert_eq!(text(&msg).len(), TRAVSR_RESPONSE_CAP);
    assert!(!text(&msg).contains("truncated"));
}

#[tokio::test]
async fn s7_an_unreachable_travsr_is_a_tool_error_with_a_short_reason() {
    let s = setup(&["get_callers"]);
    s.host.set(Answer::Fail(format!(
        "travsr exited: {}",
        "x".repeat(5_000)
    )));
    let msg = ask(&s, &call(1, "get_callers", json!({ "symbol": "main" }))).await;
    assert!(is_error(&msg));
    let reason = text(&msg);
    assert!(reason.starts_with("Travsr"), "{reason}");
    assert!(reason.contains("travsr exited"), "{reason}");
    assert!(reason.len() < 600, "{}", reason.len());
}

#[tokio::test(start_paused = true)]
async fn s7_a_travsr_that_does_not_answer_times_out_as_a_tool_error() {
    let s = setup_with(&["get_callers"], Duration::from_secs(30));
    s.host.set(Answer::Hang);
    let msg = ask(&s, &call(1, "get_callers", json!({ "symbol": "main" }))).await;
    assert!(is_error(&msg));
    let reason = text(&msg);
    assert!(reason.contains("did not answer within 30 s"), "{reason}");
    // The route still answers afterwards.
    s.host.set(Answer::Echo);
    let msg = ask(&s, &call(2, "get_callers", json!({ "symbol": "main" }))).await;
    assert!(!is_error(&msg));
}

/// Sends `message` as run A, and drops the request after 200 ms, as a client that hangs up.
async fn hang_up(s: &Setup, message: &Value) {
    let gave_up = tokio::time::timeout(Duration::from_millis(200), ask(s, message)).await;
    assert!(gave_up.is_err(), "the host answered");
}

/// Waits up to 2 s for the host's hanging work to be dropped.
async fn until_dropped(host: &FakeTravsr) -> bool {
    for _ in 0..200 {
        if host.hung_dropped.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn s7_a_call_whose_request_is_dropped_is_stopped_with_it() {
    // A call timeout far away: only dropping the request can stop the call.
    let s = setup_with(&["get_callers"], Duration::from_secs(3600));
    s.host.set(Answer::Hang);
    hang_up(&s, &call(1, "get_callers", json!({ "symbol": "main" }))).await;
    assert!(until_dropped(&s.host).await, "the call kept running");
}

#[tokio::test]
async fn a_listing_whose_request_is_dropped_is_stopped_with_it() {
    let s = setup_with(&["get_callers"], Duration::from_secs(3600));
    s.host.set(Answer::Hang);
    hang_up(&s, &rpc(json!(1), "tools/list", json!({}))).await;
    assert!(until_dropped(&s.host).await, "the listing kept running");
}

#[tokio::test]
async fn s7_travsr_s_own_error_reaches_the_agent_as_a_tool_error() {
    let s = setup(&["get_callers"]);
    s.host.set(Answer::ToolError("symbol not found".into()));
    let msg = ask(&s, &call(1, "get_callers", json!({ "symbol": "nope" }))).await;
    assert!(is_error(&msg));
    assert_eq!(text(&msg), "symbol not found");
}

#[tokio::test]
async fn a_host_that_panics_is_a_tool_error() {
    let s = setup(&["get_callers"]);
    s.host.set(Answer::Panic);
    let msg = ask(&s, &call(1, "get_callers", json!({ "symbol": "main" }))).await;
    assert!(is_error(&msg));
    assert!(text(&msg).contains("failed inside Kestrel"));
}

#[tokio::test]
async fn an_answer_without_content_is_passed_on_as_text() {
    let s = setup(&["get_callers"]);
    s.host.set(Answer::Bare);
    let msg = ask(&s, &call(1, "get_callers", json!({ "symbol": "main" }))).await;
    assert!(!is_error(&msg));
    assert_eq!(text(&msg), json!({ "answer": 42 }).to_string());
}

#[tokio::test]
async fn s9_an_unknown_token_is_401_and_never_echoed() {
    let s = setup(&["get_callers"]);
    let response = send(&s.app, "rt_live_nobody", &call(1, "get_callers", json!({}))).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let msg = body(response).await;
    assert_eq!(msg["error"]["code"], -32001);
    assert!(!msg.to_string().contains("rt_live_nobody"));
    assert!(s.host.calls().is_empty());
}

#[tokio::test]
async fn s9_run_a_s_token_only_ever_reaches_run_a() {
    let s = setup(&["get_callers"]);
    // Whatever the request names, the run is the token's.
    let msg = ask(
        &s,
        &call(
            1,
            "get_callers",
            json!({ "symbol": "main", "run": s.b.to_string(), "repo": "/runs/b" }),
        ),
    )
    .await;
    assert!(!is_error(&msg));
    let response = send(
        &s.app,
        TOKEN_B,
        &call(2, "get_callers", json!({ "symbol": "x" })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let calls = s.host.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, s.a);
    assert_eq!(calls[1].0, s.b);
    assert!(calls[0].2.get("repo").is_none());
}

#[tokio::test]
async fn a_finished_run_is_410_and_an_unsure_host_503() {
    let s = setup(&["get_callers"]);
    let response = send(&s.app, FINISHED, &rpc(json!(1), "tools/list", json!({}))).await;
    assert_eq!(response.status(), StatusCode::GONE);
    let response = send(&s.app, BROKEN, &rpc(json!(1), "tools/list", json!({}))).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn bad_requests_are_json_rpc_errors() {
    let s = setup(&["get_callers"]);
    let no_name = rpc(json!(1), "tools/call", json!({ "arguments": {} }));
    assert_eq!(ask(&s, &no_name).await["error"]["code"], -32602);
    let bad_args = rpc(
        json!(1),
        "tools/call",
        json!({ "name": "get_callers", "arguments": "symbol=main" }),
    );
    assert_eq!(ask(&s, &bad_args).await["error"]["code"], -32602);
    // No arguments at all is an empty object.
    let bare = rpc(json!(3), "tools/call", json!({ "name": "get_callers" }));
    assert!(!is_error(&ask(&s, &bare).await));
    assert_eq!(s.host.calls()[0].2, json!({}));
    let response = send_raw(&s.app, TOKEN_A, b"{not json".to_vec()).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // A notification is accepted and answered with nothing.
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert_eq!(
        send(&s.app, TOKEN_A, &note).await.status(),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn a_body_over_1_mib_is_413() {
    let s = setup(&["get_callers"]);
    let big = call(1, "get_callers", json!({ "symbol": "x".repeat(1_100_000) }));
    let response = send(&s.app, TOKEN_A, &big).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(s.host.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn calls_share_the_run_s_rate_limit() {
    let s = setup(&["get_callers"]);
    let mut limited = 0;
    for i in 0..25 {
        let response = send(&s.app, TOKEN_A, &call(i, "get_callers", json!({}))).await;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
        }
    }
    assert_eq!(limited, 5);
}

#[tokio::test]
async fn get_is_405() {
    let s = setup(&["get_callers"]);
    let request = Request::get(format!("/r/{TOKEN_A}/travsr"))
        .body(Body::empty())
        .unwrap();
    let response = s.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn s11_without_travsr_the_route_is_404_and_mcp_lists_no_travsr_tool() {
    let run = RunId::new();
    let deps = GatewayDeps {
        resolver: Arc::new(Tokens(HashMap::from([(TOKEN_A.to_string(), run)]))),
        budget: Arc::new(FakeBudget::new(true)),
        meter: Arc::new(FakeSink::default()),
        tools: Arc::new(AllKestrel),
        travsr: None,
    };
    let app = router(config("http://127.0.0.1:9", "http://127.0.0.1:9"), deps);
    let response = send(&app, TOKEN_A, &rpc(json!(1), "tools/list", json!({}))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let request = Request::post(format!("/r/{TOKEN_A}/mcp"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            rpc(json!(1), "tools/list", json!({})).to_string(),
        ))
        .unwrap();
    let msg = body(app.oneshot(request).await.unwrap()).await;
    let names: Vec<&str> = msg["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["kestrel.spawn", "kestrel.join", "kestrel.ask_human"]
    );
}

/// Every Kestrel tool, so the `/mcp` listing is not empty by accident.
struct AllKestrel;

#[async_trait]
impl kestrel_gateway::ToolHost for AllKestrel {
    async fn tools(&self, _: RunId) -> Result<Vec<KestrelTool>, ToolsError> {
        Ok(KestrelTool::ALL.to_vec())
    }
    async fn spawn(&self, _: RunId, _: kestrel_gateway::SpawnCall) -> Result<Value, String> {
        Err("unused".into())
    }
    async fn join(&self, _: RunId, _: kestrel_gateway::JoinCall) -> Result<Value, String> {
        Err("unused".into())
    }
    async fn ask_human(&self, _: RunId, _: String) -> Result<String, String> {
        Err("unused".into())
    }
    async fn denied(&self, _: RunId, _: &str) {}
}

#[test]
fn the_allowlist_is_the_ten_read_tools() {
    assert_eq!(
        TRAVSR_TOOLS,
        [
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
        ]
    );
    assert_eq!(TRAVSR_RESPONSE_CAP, 256 * 1024);
}
