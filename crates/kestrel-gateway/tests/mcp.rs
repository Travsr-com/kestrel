//! Issue #42: the Kestrel tools served to sandboxes over MCP at `/r/{token}/mcp`, against a fake
//! `ToolHost`. Requests go through the router in process, so a paused clock drives the keepalives.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{FakeBudget, FakeSink, config};
use futures::StreamExt;
use kestrel_gateway::{
    GatewayDeps, JoinCall, JoinWait, KestrelTool, RunResolver, SpawnCall, ToolHost, ToolsError,
    router,
};
use kestrel_types::{MicroUsd, RunId};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN_A: &str = "rt_live_mcp_a";
const TOKEN_B: &str = "rt_live_mcp_b";
const FINISHED: &str = "rt_live_mcp_done";

/// Resolves several tokens.
struct Tokens(HashMap<String, RunId>);

#[async_trait]
impl RunResolver for Tokens {
    async fn resolve(&self, token: &str) -> Option<RunId> {
        self.0.get(token).copied()
    }
}

/// A fake host: which tools each run may call, what each tool answers, and every call it got.
#[derive(Default)]
struct FakeHost {
    allowed: Mutex<HashMap<RunId, Vec<KestrelTool>>>,
    gone: Mutex<Vec<RunId>>,
    unavailable: Mutex<Vec<RunId>>,
    spawns: Mutex<Vec<(RunId, SpawnCall)>>,
    joins: Mutex<Vec<(RunId, JoinCall)>>,
    asks: Mutex<Vec<(RunId, String)>>,
    denied: Mutex<Vec<(RunId, String)>>,
    tools_asked: Mutex<usize>,
    spawn_refusal: Mutex<Option<String>>,
    join_takes: Mutex<Duration>,
}

#[async_trait]
impl ToolHost for FakeHost {
    async fn tools(&self, run: RunId) -> Result<Vec<KestrelTool>, ToolsError> {
        *self.tools_asked.lock().unwrap() += 1;
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
    async fn spawn(&self, run: RunId, call: SpawnCall) -> Result<Value, String> {
        self.spawns.lock().unwrap().push((run, call.clone()));
        if let Some(reason) = self.spawn_refusal.lock().unwrap().clone() {
            return Err(reason);
        }
        Ok(json!({ "child": "00000000-0000-7000-8000-000000000001", "parent": run.to_string() }))
    }
    async fn join(&self, run: RunId, call: JoinCall) -> Result<Value, String> {
        self.joins.lock().unwrap().push((run, call));
        let takes = *self.join_takes.lock().unwrap();
        tokio::time::sleep(takes).await;
        Ok(json!({ "children": [{ "outcome": "completed", "summary": "done" }] }))
    }
    async fn ask_human(&self, run: RunId, question: String) -> Result<String, String> {
        self.asks.lock().unwrap().push((run, question));
        Ok("release/2.4".into())
    }
    async fn denied(&self, run: RunId, tool: &str) {
        self.denied.lock().unwrap().push((run, tool.to_string()));
    }
}

struct Setup {
    app: Router,
    host: Arc<FakeHost>,
    a: RunId,
    b: RunId,
    done: RunId,
}

fn setup(allowed_a: &[KestrelTool]) -> Setup {
    let (a, b, done) = (RunId::new(), RunId::new(), RunId::new());
    let tokens = Tokens(HashMap::from([
        (TOKEN_A.to_string(), a),
        (TOKEN_B.to_string(), b),
        (FINISHED.to_string(), done),
    ]));
    let host = Arc::new(FakeHost::default());
    host.allowed.lock().unwrap().insert(a, allowed_a.to_vec());
    host.allowed
        .lock()
        .unwrap()
        .insert(b, KestrelTool::ALL.to_vec());
    host.gone.lock().unwrap().push(done);
    let deps = GatewayDeps {
        resolver: Arc::new(tokens),
        budget: Arc::new(FakeBudget::new(true)),
        meter: Arc::new(FakeSink::default()),
        tools: host.clone(),
    };
    let app = router(config("http://127.0.0.1:9", "http://127.0.0.1:9"), deps);
    Setup {
        app,
        host,
        a,
        b,
        done,
    }
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

/// Posts raw bytes; `sse` asks for an event stream as MCP clients do.
async fn send(app: &Router, token: &str, body: Vec<u8>, sse: bool) -> axum::response::Response {
    let accept = if sse {
        "application/json, text/event-stream"
    } else {
        "application/json"
    };
    let request = Request::post(format!("/r/{token}/mcp"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, accept)
        .body(Body::from(body))
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

async fn text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The JSON-RPC message of a JSON or an SSE response.
async fn message(response: axum::response::Response) -> Value {
    let sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.to_str().unwrap().starts_with("text/event-stream"));
    let body = text(response).await;
    if !sse {
        return serde_json::from_str(&body).unwrap();
    }
    let data = body
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .expect("an SSE data line");
    serde_json::from_str(data).unwrap()
}

/// The tool result's content as JSON (its one text item).
fn content(msg: &Value) -> Value {
    let text = msg["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

// 1
#[tokio::test]
async fn initialize_says_who_it_is_and_that_it_serves_tools() {
    let s = setup(&KestrelTool::ALL);
    let init = rpc(
        json!(1),
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }),
    );
    let response = send(&s.app, TOKEN_A, init.to_string().into_bytes(), true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let msg = message(response).await;
    assert_eq!(msg["id"], json!(1));
    assert_eq!(
        msg["result"]["protocolVersion"],
        json!(kestrel_gateway::MCP_PROTOCOL_VERSION)
    );
    assert_eq!(msg["result"]["serverInfo"]["name"], json!("kestrel"));
    assert!(
        msg["result"]["serverInfo"]["version"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert!(msg["result"]["capabilities"]["tools"].is_object());
    // The client's "initialized" notification is accepted with no body.
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let response = send(&s.app, TOKEN_A, note.to_string().into_bytes(), true).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(text(response).await.is_empty());
}

// 2
#[tokio::test]
async fn tools_list_shows_only_what_the_run_may_call() {
    let s = setup(&[KestrelTool::Spawn, KestrelTool::Join]);
    let response = send(
        &s.app,
        TOKEN_A,
        rpc(json!("l"), "tools/list", json!({}))
            .to_string()
            .into_bytes(),
        false,
    )
    .await;
    let msg = message(response).await;
    let tools = msg["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["kestrel.spawn", "kestrel.join"]);
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], json!("object"));
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
    }
}

// 3, 14
#[tokio::test]
async fn an_unknown_token_is_401_and_reaches_nothing() {
    let s = setup(&KestrelTool::ALL);
    let secret = "rt_live_not_a_real_token_123";
    let response = send(
        &s.app,
        secret,
        call(1, "kestrel.spawn", json!({ "agent": "w", "input": "x" }))
            .to_string()
            .into_bytes(),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = text(response).await;
    assert!(!body.contains(secret), "{body}");
    assert_eq!(*s.host.tools_asked.lock().unwrap(), 0);
    assert!(s.host.spawns.lock().unwrap().is_empty());
}

// 4, 14
#[tokio::test]
async fn a_finished_run_is_410_and_spawns_nothing() {
    let s = setup(&KestrelTool::ALL);
    let response = send(
        &s.app,
        FINISHED,
        call(1, "kestrel.spawn", json!({ "agent": "w", "input": "x" }))
            .to_string()
            .into_bytes(),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::GONE);
    let body = text(response).await;
    assert!(!body.contains(FINISHED), "{body}");
    assert!(s.host.spawns.lock().unwrap().is_empty());
    let _ = s.done;
}

// 5, 12
#[tokio::test]
async fn spawn_acts_as_the_run_of_the_token_never_one_from_the_body() {
    let s = setup(&KestrelTool::ALL);
    let args = json!({ "agent": "worker", "input": "fix the tests", "budget_usd": "0.50", "wait": false, "run": s.b.to_string() });
    let msg = message(
        send(
            &s.app,
            TOKEN_A,
            call(7, "kestrel.spawn", args).to_string().into_bytes(),
            true,
        )
        .await,
    )
    .await;
    assert_eq!(msg["id"], json!(7));
    assert_eq!(msg["result"]["isError"], json!(false));
    assert_eq!(
        content(&msg)["child"],
        json!("00000000-0000-7000-8000-000000000001")
    );
    let spawns = s.host.spawns.lock().unwrap().clone();
    assert_eq!(spawns.len(), 1);
    let (run, got) = &spawns[0];
    assert_eq!(*run, s.a);
    assert_eq!(got.agent, "worker");
    assert_eq!(got.input, "fix the tests");
    assert_eq!(got.budget, Some(MicroUsd(500_000)));
    assert!(!got.wait);
    // The same call through the other token acts as the other run.
    message(
        send(
            &s.app,
            TOKEN_B,
            call(
                8,
                "kestrel.spawn",
                json!({ "agent": "worker", "input": "y", "budget_usd": 1 }),
            )
            .to_string()
            .into_bytes(),
            true,
        )
        .await,
    )
    .await;
    let spawns = s.host.spawns.lock().unwrap().clone();
    assert_eq!(spawns[1].0, s.b);
    assert_eq!(spawns[1].1.budget, Some(MicroUsd(1_000_000)));
}

// 6
#[tokio::test]
async fn a_tool_outside_the_grant_is_not_permitted_and_recorded() {
    let s = setup(&[KestrelTool::Join]);
    let msg = message(
        send(
            &s.app,
            TOKEN_A,
            call(1, "kestrel.spawn", json!({ "agent": "w", "input": "x" }))
                .to_string()
                .into_bytes(),
            true,
        )
        .await,
    )
    .await;
    assert_eq!(msg["result"]["isError"], json!(true));
    assert!(
        content(&msg).as_str().unwrap().contains("not permitted"),
        "{msg}"
    );
    assert!(s.host.spawns.lock().unwrap().is_empty());
    assert_eq!(
        *s.host.denied.lock().unwrap(),
        [(s.a, "kestrel.spawn".to_string())]
    );
}

// 7
#[tokio::test]
async fn a_refused_spawn_tells_the_model_why() {
    let s = setup(&KestrelTool::ALL);
    *s.host.spawn_refusal.lock().unwrap() =
        Some("the parent has $0.10 left, $0.50 was asked".into());
    let msg = message(
        send(
            &s.app,
            TOKEN_A,
            call(
                1,
                "kestrel.spawn",
                json!({ "agent": "w", "input": "x", "budget_usd": "0.5" }),
            )
            .to_string()
            .into_bytes(),
            true,
        )
        .await,
    )
    .await;
    assert_eq!(msg["result"]["isError"], json!(true));
    assert!(
        content(&msg).as_str().unwrap().contains("$0.10 left"),
        "{msg}"
    );
}

// 8
#[tokio::test(start_paused = true)]
async fn a_long_join_sends_keepalives_every_15_s_then_its_result() {
    let s = setup(&KestrelTool::ALL);
    // A little over two keepalive periods.
    *s.host.join_takes.lock().unwrap() = Duration::from_secs(31);
    let response = send(
        &s.app,
        TOKEN_A,
        call(3, "kestrel.join", json!({ "mode": "all", "timeout_s": 60 }))
            .to_string()
            .into_bytes(),
        true,
    )
    .await;
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let started = tokio::time::Instant::now();
    let mut stream = response.into_body().into_data_stream();
    let mut pings = Vec::new();
    let mut result = None;
    while let Some(chunk) = stream.next().await {
        let chunk = String::from_utf8(chunk.unwrap().to_vec()).unwrap();
        if chunk.starts_with(':') {
            pings.push(started.elapsed());
        } else if let Some(data) = chunk.lines().find_map(|l| l.strip_prefix("data: ")) {
            result = Some(serde_json::from_str::<Value>(data).unwrap());
        }
    }
    assert!(pings.len() >= 2, "{pings:?}");
    assert!(
        pings.iter().all(|p| *p >= Duration::from_secs(15)),
        "{pings:?}"
    );
    let msg = result.expect("the join result");
    assert_eq!(content(&msg)["children"][0]["outcome"], json!("completed"));
    let joins = s.host.joins.lock().unwrap().clone();
    assert_eq!(joins[0].1.wait, JoinWait::All);
    assert_eq!(joins[0].1.timeout, Some(Duration::from_secs(60)));
    assert!(joins[0].1.children.is_empty());
}

// 9
#[tokio::test(start_paused = true)]
async fn a_dropped_call_called_again_gets_the_same_result_without_a_second_join() {
    let s = setup(&KestrelTool::ALL);
    *s.host.join_takes.lock().unwrap() = Duration::from_secs(30);
    let body = call(11, "kestrel.join", json!({})).to_string().into_bytes();
    // The client goes away mid-call.
    let first = send(&s.app, TOKEN_A, body.clone(), true).await;
    drop(first);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let msg = message(send(&s.app, TOKEN_A, body, true).await).await;
    assert_eq!(content(&msg)["children"][0]["summary"], json!("done"));
    assert_eq!(s.host.joins.lock().unwrap().len(), 1);
    // The same id from another run is that run's own call.
    let other = message(
        send(
            &s.app,
            TOKEN_B,
            call(11, "kestrel.join", json!({})).to_string().into_bytes(),
            true,
        )
        .await,
    )
    .await;
    assert_eq!(other["result"]["isError"], json!(false));
    assert_eq!(s.host.joins.lock().unwrap().len(), 2);
}

// 10 (the engine side is tested in kestrel-core)
#[tokio::test]
async fn ask_human_returns_the_answer_text() {
    let s = setup(&KestrelTool::ALL);
    let msg = message(
        send(
            &s.app,
            TOKEN_A,
            call(
                2,
                "kestrel.ask_human",
                json!({ "question": "Which branch?" }),
            )
            .to_string()
            .into_bytes(),
            true,
        )
        .await,
    )
    .await;
    assert_eq!(content(&msg), json!("release/2.4"));
    assert_eq!(
        *s.host.asks.lock().unwrap(),
        [(s.a, "Which branch?".to_string())]
    );
}

// 11
#[tokio::test]
async fn a_body_over_1_mb_is_413_and_bad_json_rpc_is_an_error_not_a_500() {
    let s = setup(&KestrelTool::ALL);
    let big = vec![b' '; 1024 * 1024 + 1];
    assert_eq!(
        send(&s.app, TOKEN_A, big, true).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let cases: [(&[u8], i64); 6] = [
        (b"{not json", -32700),
        (b"[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}]", -32600),
        (b"{\"jsonrpc\":\"2.0\",\"id\":1}", -32600),
        (b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"resources/list\"}", -32601),
        (b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{}}", -32602),
        (b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"rm\"}}", -32602),
    ];
    for (body, code) in cases {
        let response = send(&s.app, TOKEN_A, body.to_vec(), true).await;
        assert!(
            response.status().is_client_error() || response.status().is_success(),
            "{}",
            response.status()
        );
        let msg = message(response).await;
        assert_eq!(msg["error"]["code"], json!(code), "{msg}");
    }
}

#[tokio::test]
async fn tool_arguments_are_checked() {
    let s = setup(&KestrelTool::ALL);
    let bad = [
        ("kestrel.spawn", json!({ "input": "x" })),
        (
            "kestrel.spawn",
            json!({ "agent": "w", "input": "x", "budget_usd": "abc" }),
        ),
        (
            "kestrel.spawn",
            json!({ "agent": "w", "input": "x", "budget_usd": -1 }),
        ),
        ("kestrel.join", json!({ "mode": "some" })),
        ("kestrel.join", json!({ "children": ["not-a-run-id"] })),
        ("kestrel.join", json!({ "timeout_s": -5 })),
        ("kestrel.ask_human", json!({})),
        ("kestrel.ask_human", json!({ "question": "  " })),
    ];
    for (i, (tool, args)) in bad.into_iter().enumerate() {
        let msg = message(
            send(
                &s.app,
                TOKEN_A,
                call(i as u64, tool, args.clone()).to_string().into_bytes(),
                true,
            )
            .await,
        )
        .await;
        assert_eq!(msg["error"]["code"], json!(-32602), "{tool} {args}: {msg}");
    }
    assert!(s.host.spawns.lock().unwrap().is_empty());
    assert!(s.host.joins.lock().unwrap().is_empty());
    assert!(s.host.asks.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn tool_calls_are_rate_limited_per_run() {
    let s = setup(&KestrelTool::ALL);
    let mut limited = 0;
    for i in 0..25 {
        let response = send(
            &s.app,
            TOKEN_A,
            call(100 + i, "kestrel.ask_human", json!({ "question": "q" }))
                .to_string()
                .into_bytes(),
            false,
        )
        .await;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
            assert_eq!(message(response).await["error"]["code"], json!(-32000));
        }
    }
    assert_eq!(limited, 5);
    // Another run has its own allowance.
    let other = send(
        &s.app,
        TOKEN_B,
        call(1, "kestrel.ask_human", json!({ "question": "q" }))
            .to_string()
            .into_bytes(),
        false,
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);
    // Time refills it.
    tokio::time::advance(Duration::from_secs(1)).await;
    let later = send(
        &s.app,
        TOKEN_A,
        call(200, "kestrel.ask_human", json!({ "question": "q" }))
            .to_string()
            .into_bytes(),
        false,
    )
    .await;
    assert_eq!(later.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_json_only_client_gets_a_json_answer() {
    let s = setup(&KestrelTool::ALL);
    let response = send(
        &s.app,
        TOKEN_A,
        call(1, "kestrel.ask_human", json!({ "question": "q" }))
            .to_string()
            .into_bytes(),
        false,
    )
    .await;
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(content(&message(response).await), json!("release/2.4"));
}

#[tokio::test]
async fn ping_answers_with_an_empty_result() {
    let s = setup(&KestrelTool::ALL);
    let msg = message(
        send(
            &s.app,
            TOKEN_A,
            rpc(json!(9), "ping", json!({})).to_string().into_bytes(),
            false,
        )
        .await,
    )
    .await;
    assert_eq!(msg, json!({ "jsonrpc": "2.0", "id": 9, "result": {} }));
}

#[test]
fn every_tool_has_its_mcp_name() {
    let names: Vec<&str> = KestrelTool::ALL.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        ["kestrel.spawn", "kestrel.join", "kestrel.ask_human"]
    );
    assert_eq!(
        KestrelTool::from_name("kestrel.join"),
        Some(KestrelTool::Join)
    );
    assert_eq!(KestrelTool::from_name("kestrel.nope"), None);
}

#[tokio::test]
async fn get_offers_no_stream() {
    let s = setup(&KestrelTool::ALL);
    let request = Request::get(format!("/r/{TOKEN_A}/mcp"))
        .body(Body::empty())
        .unwrap();
    let response = s.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers()[header::ALLOW], "POST");
}

// Review 1 of PR 76: what a sandbox can make the gateway hold, and protocol details.

#[tokio::test]
async fn an_id_that_is_not_a_short_string_or_an_integer_is_refused() {
    let s = setup(&KestrelTool::ALL);
    let long = "x".repeat(200);
    for id in [
        json!(long),
        json!(1.5),
        json!({ "a": 1 }),
        json!([1]),
        json!(true),
    ] {
        let body = rpc(
            id.clone(),
            "tools/call",
            json!({ "name": "kestrel.ask_human", "arguments": { "question": "q" } }),
        );
        let msg = message(send(&s.app, TOKEN_A, body.to_string().into_bytes(), false).await).await;
        assert_eq!(msg["error"]["code"], json!(-32600), "{id}: {msg}");
    }
    assert!(s.host.asks.lock().unwrap().is_empty());
    // Short strings and integers are fine.
    for id in [json!("abc-1"), json!(42), json!(-3)] {
        let body = rpc(
            id.clone(),
            "tools/call",
            json!({ "name": "kestrel.ask_human", "arguments": { "question": "q" } }),
        );
        let msg = message(send(&s.app, TOKEN_A, body.to_string().into_bytes(), false).await).await;
        assert_eq!(msg["id"], id);
    }
}

#[tokio::test(start_paused = true)]
async fn a_run_has_at_most_8_calls_waiting() {
    let s = setup(&KestrelTool::ALL);
    *s.host.join_takes.lock().unwrap() = Duration::from_secs(600);
    let mut waiting = Vec::new();
    for i in 0..8 {
        waiting.push(
            send(
                &s.app,
                TOKEN_A,
                call(500 + i, "kestrel.join", json!({}))
                    .to_string()
                    .into_bytes(),
                true,
            )
            .await,
        );
    }
    let ninth = send(
        &s.app,
        TOKEN_A,
        call(600, "kestrel.join", json!({}))
            .to_string()
            .into_bytes(),
        true,
    )
    .await;
    assert_eq!(ninth.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        message(ninth).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("waiting")
    );
    // Another run is not held up by these.
    let other = send(
        &s.app,
        TOKEN_B,
        call(1, "kestrel.ask_human", json!({ "question": "q" }))
            .to_string()
            .into_bytes(),
        false,
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);
    // Once they are done, there is room again.
    tokio::time::sleep(Duration::from_secs(601)).await;
    drop(waiting);
    let later = send(
        &s.app,
        TOKEN_A,
        call(700, "kestrel.ask_human", json!({ "question": "q" }))
            .to_string()
            .into_bytes(),
        false,
    )
    .await;
    assert_eq!(later.status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn a_run_keeps_at_most_256_finished_results() {
    let s = setup(&KestrelTool::ALL);
    for i in 0..300u64 {
        // Spread out so the rate limit never answers.
        tokio::time::advance(Duration::from_millis(200)).await;
        let msg = message(
            send(
                &s.app,
                TOKEN_A,
                call(i, "kestrel.ask_human", json!({ "question": "q" }))
                    .to_string()
                    .into_bytes(),
                false,
            )
            .await,
        )
        .await;
        assert_eq!(content(&msg), json!("release/2.4"));
    }
    assert_eq!(s.host.asks.lock().unwrap().len(), 300);
    // The newest result is still kept: calling again gives it without asking again.
    message(
        send(
            &s.app,
            TOKEN_A,
            call(299, "kestrel.ask_human", json!({ "question": "q" }))
                .to_string()
                .into_bytes(),
            false,
        )
        .await,
    )
    .await;
    assert_eq!(s.host.asks.lock().unwrap().len(), 300);
    // The oldest was let go: calling again asks again.
    message(
        send(
            &s.app,
            TOKEN_A,
            call(0, "kestrel.ask_human", json!({ "question": "q" }))
                .to_string()
                .into_bytes(),
            false,
        )
        .await,
    )
    .await;
    assert_eq!(s.host.asks.lock().unwrap().len(), 301);
}

#[tokio::test]
async fn join_takes_at_most_256_children() {
    let s = setup(&KestrelTool::ALL);
    let children: Vec<String> = (0..257).map(|_| RunId::new().to_string()).collect();
    let msg = message(
        send(
            &s.app,
            TOKEN_A,
            call(1, "kestrel.join", json!({ "children": children }))
                .to_string()
                .into_bytes(),
            false,
        )
        .await,
    )
    .await;
    assert_eq!(msg["error"]["code"], json!(-32602));
    assert!(s.host.joins.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_host_that_cannot_say_which_tools_is_503_not_a_denial() {
    let s = setup(&KestrelTool::ALL);
    s.host.unavailable.lock().unwrap().push(s.a);
    let response = send(
        &s.app,
        TOKEN_A,
        call(1, "kestrel.spawn", json!({ "agent": "w", "input": "x" }))
            .to_string()
            .into_bytes(),
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(s.host.denied.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_client_response_and_a_wrong_version_are_handled() {
    let s = setup(&KestrelTool::ALL);
    // A JSON-RPC response from the client (to a server request) is accepted with no body.
    let reply = json!({ "jsonrpc": "2.0", "id": 5, "result": {} });
    let response = send(&s.app, TOKEN_A, reply.to_string().into_bytes(), false).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    // Not JSON-RPC 2.0.
    let old = json!({ "jsonrpc": "1.0", "id": 5, "method": "ping" });
    let msg = message(send(&s.app, TOKEN_A, old.to_string().into_bytes(), false).await).await;
    assert_eq!(msg["error"]["code"], json!(-32600));
}

#[tokio::test]
async fn initialize_answers_the_version_the_client_asked_for_when_it_is_supported() {
    let s = setup(&KestrelTool::ALL);
    for (asked, answered) in [
        ("2025-03-26", "2025-03-26"),
        ("2025-06-18", "2025-06-18"),
        ("1999-01-01", kestrel_gateway::MCP_PROTOCOL_VERSION),
    ] {
        let init = rpc(
            json!(1),
            "initialize",
            json!({ "protocolVersion": asked, "capabilities": {} }),
        );
        let msg = message(send(&s.app, TOKEN_A, init.to_string().into_bytes(), false).await).await;
        assert_eq!(msg["result"]["protocolVersion"], json!(answered), "{asked}");
    }
}

#[tokio::test]
async fn more_argument_shapes_are_checked() {
    let s = setup(&KestrelTool::ALL);
    let bad = [
        (
            "kestrel.spawn",
            json!({ "agent": "w", "input": "x", "budget_usd": true }),
        ),
        ("kestrel.join", json!({ "children": "not-a-list" })),
        ("kestrel.join", json!({ "timeout_s": "soon" })),
    ];
    for (i, (tool, args)) in bad.into_iter().enumerate() {
        let msg = message(
            send(
                &s.app,
                TOKEN_A,
                call(i as u64, tool, args.clone()).to_string().into_bytes(),
                false,
            )
            .await,
        )
        .await;
        assert_eq!(msg["error"]["code"], json!(-32602), "{tool} {args}: {msg}");
    }
}

#[tokio::test]
async fn no_tools_offers_nothing_and_does_nothing() {
    use kestrel_gateway::NoTools;
    let run = RunId::new();
    assert_eq!(NoTools.tools(run).await, Ok(vec![]));
    let spawn = SpawnCall {
        agent: "w".into(),
        input: "x".into(),
        budget: None,
        wait: false,
    };
    assert!(NoTools.spawn(run, spawn).await.is_err());
    let join = JoinCall {
        children: vec![],
        wait: JoinWait::All,
        timeout: None,
    };
    assert!(NoTools.join(run, join).await.is_err());
    assert!(NoTools.ask_human(run, "q".into()).await.is_err());
    NoTools.denied(run, "kestrel.spawn").await;
}
