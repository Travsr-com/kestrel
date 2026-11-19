//! Scenario tests for issue #4: pass-through LLM gateway with per-run cost metering.
//!
//! Upstream providers are mocked with `wiremock`, except scenario 2, which needs an upstream
//! that sends SSE events with delays between them (wiremock can only delay a whole response),
//! so it uses a small axum server instead.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use futures::StreamExt;
use kestrel_types::{MicroUsd, ModelCallRecord};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SSE: &str = "text/event-stream";
const JSON: &str = "application/json";

async fn mock_json(server: &MockServer, p: &str, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(status).set_body_raw(body.as_bytes().to_vec(), JSON))
        .mount(server)
        .await;
}

async fn mock_sse(server: &MockServer, p: &str, events: &[String]) {
    Mock::given(method("POST"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_raw(events.concat().into_bytes(), SSE))
        .mount(server)
        .await;
}

fn single(records: Vec<ModelCallRecord>) -> ModelCallRecord {
    assert_eq!(
        records.len(),
        1,
        "expected exactly one record, got {records:?}"
    );
    records.into_iter().next().unwrap()
}

// ---------------------------------------------------------------------------
// Scenario 1
// ---------------------------------------------------------------------------

/// Given a non-streaming Anthropic request, then the upstream receives the same body and
/// the client receives the upstream body byte for byte.
#[tokio::test]
async fn s1_non_streaming_anthropic_body_passes_through_byte_for_byte() {
    let upstream = MockServer::start().await;
    let resp_body = anthropic_response(CLAUDE_MODEL, 12, 6, 0, 0);
    mock_json(&upstream, "/v1/messages", 200, &resp_body).await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let req_body = anthropic_request(CLAUDE_MODEL, false);
    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .header("anthropic-version", "2023-06-01")
        .body(req_body.clone())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let got = resp.bytes().await.unwrap();
    assert_eq!(
        got.as_ref(),
        resp_body.as_bytes(),
        "client body must be upstream body byte for byte"
    );

    let reqs = upstream.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].url.path(), "/v1/messages");
    assert_eq!(
        reqs[0].body,
        req_body.as_bytes(),
        "upstream must receive the same request body"
    );
    assert_eq!(
        reqs[0]
            .headers
            .get("anthropic-version")
            .map(|v| v.to_str().unwrap()),
        Some("2023-06-01"),
        "anthropic-version header is forwarded"
    );
}

// ---------------------------------------------------------------------------
// Scenario 2
// ---------------------------------------------------------------------------

/// Upstream that sends each SSE event with a delay in between, and flips `done` once it has
/// produced the last event.
async fn start_slow_sse_upstream(events: Vec<String>, gap: Duration) -> (String, Arc<AtomicBool>) {
    let done = Arc::new(AtomicBool::new(false));
    let done_srv = done.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let events = events.clone();
            let done = done_srv.clone();
            async move {
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(16);
                tokio::spawn(async move {
                    let n = events.len();
                    for (i, ev) in events.into_iter().enumerate() {
                        if tx.send(Ok(Bytes::from(ev))).await.is_err() {
                            return;
                        }
                        if i + 1 < n {
                            tokio::time::sleep(gap).await;
                        }
                    }
                    done.store(true, Ordering::SeqCst);
                });
                axum::response::Response::builder()
                    .status(200)
                    .header("content-type", SSE)
                    .body(axum::body::Body::from_stream(
                        tokio_stream::wrappers::ReceiverStream::new(rx),
                    ))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), done)
}

/// Given a streaming Anthropic response, then the client receives every SSE event in order,
/// and the first chunk arrives before the upstream finishes (no full buffering).
#[tokio::test]
async fn s2_streaming_anthropic_events_in_order_without_full_buffering() {
    let events = anthropic_sse_events(CLAUDE_MODEL, 25, 15, 0, 0);
    let gap = Duration::from_millis(250);
    let (base, upstream_done) = start_slow_sse_upstream(events.clone(), gap).await;
    let h = start(&base, &base, true).await;

    let t0 = Instant::now();
    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .header("anthropic-version", "2023-06-01")
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with(SSE)),
        "content-type stays text/event-stream"
    );

    let mut stream = resp.bytes_stream();
    let mut received = Vec::new();
    let mut first_chunk_at = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        if first_chunk_at.is_none() && !chunk.is_empty() {
            first_chunk_at = Some(t0.elapsed());
            assert!(
                !upstream_done.load(Ordering::SeqCst),
                "first chunk reached the client only after upstream finished: response was buffered"
            );
        }
        received.extend_from_slice(&chunk);
    }
    let total = t0.elapsed();
    let first = first_chunk_at.expect("client received no chunks");
    eprintln!(
        "s2 timing: first chunk after {} ms, full stream after {} ms ({} events, {} ms gap)",
        first.as_millis(),
        total.as_millis(),
        events.len(),
        gap.as_millis()
    );

    assert_eq!(
        String::from_utf8(received).unwrap(),
        events.concat(),
        "every SSE event arrives, in order, unchanged"
    );
    assert!(upstream_done.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// Scenario 3
// ---------------------------------------------------------------------------

/// Given a streaming Anthropic response with `usage` in `message_start` and `message_delta`,
/// then the `ModelCallRecord` has the correct input, output, cache-read and cache-write tokens.
/// `message_delta.usage.output_tokens` is cumulative, so it replaces (not adds to) the initial
/// `output_tokens: 1` from `message_start`.
#[tokio::test]
async fn s3_streaming_anthropic_usage_recorded_from_message_start_and_delta() {
    let upstream = MockServer::start().await;
    let events = anthropic_sse_events(CLAUDE_MODEL, 1200, 350, 8000, 2000);
    mock_sse(&upstream, "/v1/messages", &events).await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .header("anthropic-version", "2023-06-01")
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), events.concat().as_bytes());

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(rec.run_id, h.run);
    assert_eq!(rec.provider, "anthropic");
    assert_eq!(rec.model, CLAUDE_MODEL);
    assert_eq!(rec.tokens_in, 1200);
    assert_eq!(rec.tokens_out, 350);
    assert_eq!(rec.tokens_cache_read, 8000);
    assert_eq!(rec.tokens_cache_write, 2000);
    assert_eq!(rec.status, 200);
}

/// Newer API versions also repeat cumulative input and cache counts in `message_delta.usage`.
/// They must not be double counted.
#[tokio::test]
async fn s3_streaming_anthropic_cumulative_delta_usage_not_double_counted() {
    let upstream = MockServer::start().await;
    let mut events = anthropic_sse_events(CLAUDE_MODEL, 1200, 350, 8000, 2000);
    let i = events
        .iter()
        .position(|e| e.starts_with("event: message_delta"))
        .unwrap();
    events[i] = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":1200,\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":8000,\"output_tokens\":350}}\n\n".to_string();
    mock_sse(&upstream, "/v1/messages", &events).await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(
        (
            rec.tokens_in,
            rec.tokens_out,
            rec.tokens_cache_read,
            rec.tokens_cache_write
        ),
        (1200, 350, 8000, 2000)
    );
}

// ---------------------------------------------------------------------------
// Scenario 4
// ---------------------------------------------------------------------------

/// Given an OpenAI streaming response with a final usage chunk, then tokens are recorded
/// correctly. OpenAI's `prompt_tokens` includes `prompt_tokens_details.cached_tokens`; the
/// record splits them so `tokens_in` is the uncached part and `tokens_cache_read` the cached.
#[tokio::test]
async fn s4_streaming_openai_final_usage_chunk_recorded() {
    let upstream = MockServer::start().await;
    let events = openai_sse_events(OPENAI_MODEL, 1200, 350, 1024);
    mock_sse(&upstream, "/v1/chat/completions", &events).await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(openai_request(OPENAI_MODEL, true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(
        body.as_ref(),
        events.concat().as_bytes(),
        "chunks pass through unchanged"
    );

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(rec.run_id, h.run);
    assert_eq!(rec.provider, "openai");
    assert_eq!(rec.model, OPENAI_MODEL);
    assert_eq!(rec.tokens_in, 1200 - 1024);
    assert_eq!(rec.tokens_cache_read, 1024);
    assert_eq!(rec.tokens_out, 350);
    assert_eq!(rec.tokens_cache_write, 0);
    assert_eq!(rec.status, 200);
}

// ---------------------------------------------------------------------------
// Scenario 5
// ---------------------------------------------------------------------------

/// Given a client request carrying `x-api-key: client-key`, then the upstream sees the
/// configured key, never `client-key`.
#[tokio::test]
async fn s5_anthropic_client_key_replaced_by_configured_key() {
    let upstream = MockServer::start().await;
    mock_json(
        &upstream,
        "/v1/messages",
        200,
        &anthropic_response(CLAUDE_MODEL, 1, 1, 0, 0),
    )
    .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .header("x-api-key", "client-key")
        .header("authorization", "Bearer client-key")
        .body(anthropic_request(CLAUDE_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let reqs = upstream.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let keys: Vec<_> = reqs[0].headers.get_all("x-api-key").iter().collect();
    assert_eq!(keys.len(), 1, "exactly one x-api-key upstream");
    assert_eq!(keys[0].to_str().unwrap(), ANTHROPIC_KEY);
    for (name, value) in reqs[0].headers.iter() {
        assert!(
            !value.to_str().unwrap_or("").contains("client-key"),
            "client key leaked upstream in header {name}"
        );
    }
}

/// Same rule for OpenAI, whose key travels as `Authorization: Bearer`.
#[tokio::test]
async fn s5_openai_client_key_replaced_by_configured_key() {
    let upstream = MockServer::start().await;
    mock_json(
        &upstream,
        "/v1/chat/completions",
        200,
        &openai_response(OPENAI_MODEL, 10, 2, 0),
    )
    .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .header("content-type", JSON)
        .header("authorization", "Bearer client-key")
        .header("x-api-key", "client-key")
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let reqs = upstream.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let auth: Vec<_> = reqs[0].headers.get_all("authorization").iter().collect();
    assert_eq!(auth.len(), 1, "exactly one authorization header upstream");
    assert_eq!(auth[0].to_str().unwrap(), format!("Bearer {OPENAI_KEY}"));
    for (name, value) in reqs[0].headers.iter() {
        assert!(
            !value.to_str().unwrap_or("").contains("client-key"),
            "client key leaked upstream in header {name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 6
// ---------------------------------------------------------------------------

/// Given an unknown run token, then 401 in the provider's error JSON format and nothing
/// reaches upstream.
#[tokio::test]
async fn s6_unknown_run_token_anthropic_401_and_no_upstream_call() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url("rt_unknown"))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_anthropic_error(&body, "authentication_error");

    assert_eq!(h.resolver.calls(), vec!["rt_unknown".to_string()]);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn s6_unknown_run_token_openai_401_and_no_upstream_call() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.openai_url("rt_unknown"))
        .header("content-type", JSON)
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_openai_error(&body, "invalid_request_error");

    assert_eq!(h.resolver.calls(), vec!["rt_unknown".to_string()]);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Scenario 7
// ---------------------------------------------------------------------------

/// Given `BudgetGuard::allow` is false, then 429 with a provider-format error saying the run
/// budget is used up, and nothing reaches upstream.
#[tokio::test]
async fn s7_budget_exhausted_anthropic_429_and_no_upstream_call() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), false).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_anthropic_error(&body, "rate_limit_error");
    let msg = body["error"]["message"].as_str().unwrap().to_lowercase();
    assert!(
        msg.contains("budget"),
        "message should say the run budget is used up: {msg}"
    );

    assert_eq!(h.budget.calls(), vec![h.run]);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn s7_budget_exhausted_openai_429_and_no_upstream_call() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), false).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_openai_error(&body, "insufficient_quota");
    let msg = body["error"]["message"].as_str().unwrap().to_lowercase();
    assert!(
        msg.contains("budget"),
        "message should say the run budget is used up: {msg}"
    );

    assert_eq!(h.budget.calls(), vec![h.run]);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Scenario 8
// ---------------------------------------------------------------------------

/// Given upstream returns 500, then the client gets 500 and a record is written with
/// `status: 500` and zero cost.
#[tokio::test]
async fn s8_upstream_500_passed_through_and_recorded_with_zero_cost() {
    let upstream = MockServer::start().await;
    let err_body =
        r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#;
    mock_json(&upstream, "/v1/messages", 500, err_body).await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);
    assert_eq!(
        resp.text().await.unwrap(),
        err_body,
        "upstream error body passes through"
    );

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(rec.run_id, h.run);
    assert_eq!(rec.provider, "anthropic");
    assert_eq!(rec.model, CLAUDE_MODEL);
    assert_eq!(rec.status, 500);
    assert_eq!(
        (
            rec.tokens_in,
            rec.tokens_out,
            rec.tokens_cache_read,
            rec.tokens_cache_write
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(rec.cost, Some(MicroUsd(0)));
}

// ---------------------------------------------------------------------------
// Scenario 9
// ---------------------------------------------------------------------------

/// Given a price table and known token counts, then cost is exact in `MicroUsd`, with cache
/// tokens. Prices per million tokens: in $3, out $15, cache read $0.30, cache write $3.75.
/// 1200 in = 3600, 350 out = 5250, 8000 cache read = 2400, 2000 cache write = 7500.
/// Total 18750 micro-dollars.
#[tokio::test]
async fn s9_anthropic_cost_exact_with_cache_tokens() {
    let upstream = MockServer::start().await;
    mock_json(
        &upstream,
        "/v1/messages",
        200,
        &anthropic_response(CLAUDE_MODEL, 1200, 350, 8000, 2000),
    )
    .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(
        (
            rec.tokens_in,
            rec.tokens_out,
            rec.tokens_cache_read,
            rec.tokens_cache_write
        ),
        (1200, 350, 8000, 2000)
    );
    assert_eq!(rec.cost, Some(MicroUsd(18_750)));
    assert_eq!(rec.status, 200);
}

/// Streaming path prices the same way as the non-streaming one.
#[tokio::test]
async fn s9_anthropic_streaming_cost_exact_with_cache_tokens() {
    let upstream = MockServer::start().await;
    let events = anthropic_sse_events(CLAUDE_MODEL, 1200, 350, 8000, 2000);
    mock_sse(&upstream, "/v1/messages", &events).await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    let _ = resp.bytes().await.unwrap();

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(rec.cost, Some(MicroUsd(18_750)));
}

/// OpenAI, prices per million tokens: in $0.15, out $0.60, cache read $0.075.
/// 2_000_000 prompt tokens of which 1_000_000 cached, 1_000_000 completion:
/// 1M uncached in = 150000, 1M cached = 75000, 1M out = 600000. Total 825000.
#[tokio::test]
async fn s9_openai_cost_exact_with_cached_tokens() {
    let upstream = MockServer::start().await;
    mock_json(
        &upstream,
        "/v1/chat/completions",
        200,
        &openai_response(OPENAI_MODEL, 2_000_000, 1_000_000, 1_000_000),
    )
    .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(
        (
            rec.tokens_in,
            rec.tokens_out,
            rec.tokens_cache_read,
            rec.tokens_cache_write
        ),
        (1_000_000, 1_000_000, 1_000_000, 0)
    );
    assert_eq!(rec.cost, Some(MicroUsd(825_000)));
}

/// Given a model missing from the table, then `cost: None` (tokens still recorded).
#[tokio::test]
async fn s9_unknown_model_cost_none() {
    let upstream = MockServer::start().await;
    mock_json(
        &upstream,
        "/v1/messages",
        200,
        &anthropic_response(UNPRICED_MODEL, 1200, 350, 8000, 2000),
    )
    .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(UNPRICED_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(rec.model, UNPRICED_MODEL);
    assert_eq!(rec.tokens_in, 1200);
    assert_eq!(rec.cost, None);
}

// ---------------------------------------------------------------------------
// Scenario 10
// ---------------------------------------------------------------------------

/// Given the upstream is unreachable, then 502 and an error record.
#[tokio::test]
async fn s10_unreachable_upstream_anthropic_502_and_error_record() {
    let dead = dead_base_url().await;
    let h = start(&dead, &dead, true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_anthropic_error(&body, "api_error");

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(rec.run_id, h.run);
    assert_eq!(rec.provider, "anthropic");
    assert_eq!(rec.model, CLAUDE_MODEL);
    assert_eq!(rec.status, 502);
    assert_eq!(
        (
            rec.tokens_in,
            rec.tokens_out,
            rec.tokens_cache_read,
            rec.tokens_cache_write
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(rec.cost, Some(MicroUsd(0)));
}

#[tokio::test]
async fn s10_unreachable_upstream_openai_502_and_error_record() {
    let dead = dead_base_url().await;
    let h = start(&dead, &dead, true).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_openai_error(&body, "server_error");

    let rec = single(h.sink.wait_for(1).await);
    assert_eq!(rec.provider, "openai");
    assert_eq!(rec.model, OPENAI_MODEL);
    assert_eq!(rec.status, 502);
    assert_eq!(rec.cost, Some(MicroUsd(0)));
}
