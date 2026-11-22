//! Issue #88: a run's token stops working on the gateway once the run has ended (completed,
//! failed or cancelled). A model call or tool call with it is refused with 410, nothing reaches
//! the upstream, and nothing is metered. A call already in flight when the run ends finishes and
//! is metered once.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::convert::Infallible;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use futures::StreamExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const JSON: &str = "application/json";

/// An upstream that must never be called.
async fn untouched_upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    upstream
}

// Scenario 1

#[tokio::test]
async fn an_ended_run_s_anthropic_call_is_410_reaches_nothing_and_costs_nothing() {
    let upstream = untouched_upstream().await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;
    h.resolver.end();

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
    let body: serde_json::Value = resp.json().await.unwrap();
    // Not a retryable error: the agent must not try again.
    assert_anthropic_error(&body, "permission_error");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("ended"),
        "the reason says the run ended: {msg}"
    );
    assert!(!msg.contains(RUN_TOKEN), "the token is never echoed: {msg}");

    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(h.sink.records().is_empty(), "nothing is metered");
    assert!(h.budget.calls().is_empty(), "the budget is not even asked");
}

#[tokio::test]
async fn an_ended_run_s_openai_call_is_410_reaches_nothing_and_costs_nothing() {
    let upstream = untouched_upstream().await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;
    h.resolver.end();

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_openai_error(&body, "invalid_request_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("ended"));

    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(h.sink.records().is_empty());
}

#[tokio::test]
async fn an_ended_run_s_tool_call_is_410_and_never_reaches_the_tools() {
    let upstream = untouched_upstream().await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;
    h.resolver.end();

    let resp = h
        .client
        .post(format!("http://{}/r/{RUN_TOKEN}/mcp", h.addr))
        .header("content-type", JSON)
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("ended"), "{msg}");
}

#[tokio::test]
async fn a_live_run_is_not_affected() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            anthropic_response(CLAUDE_MODEL, 10, 5, 0, 0).into_bytes(),
            JSON,
        ))
        .expect(1)
        .mount(&upstream)
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
    assert_eq!(h.sink.wait_for(1).await.len(), 1);
}

// Scenario 2

/// An upstream that streams `events` with `gap` between them.
async fn slow_upstream(events: Vec<String>, gap: Duration) -> String {
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let events = events.clone();
            async move {
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(16);
                tokio::spawn(async move {
                    for ev in events {
                        if tx.send(Ok(Bytes::from(ev))).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(gap).await;
                    }
                });
                axum::response::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
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
    format!("http://{addr}")
}

#[tokio::test]
async fn a_call_in_flight_when_the_run_ends_finishes_and_is_metered_once_and_the_next_is_refused() {
    let events = anthropic_sse_events(CLAUDE_MODEL, 100, 40, 0, 0);
    let base = slow_upstream(events.clone(), Duration::from_millis(80)).await;
    let h = start(&base, &base, true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.expect("a first chunk").unwrap();
    assert!(!first.is_empty());

    // The run ends (it was cancelled) while the call streams.
    h.resolver.end();
    let mut rest = Vec::new();
    while let Some(chunk) = stream.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    let mut all = first.to_vec();
    all.extend_from_slice(&rest);
    assert_eq!(
        String::from_utf8(all).unwrap(),
        events.concat(),
        "the call in flight is delivered whole"
    );
    let records = h.sink.wait_for(1).await;
    assert_eq!(records.len(), 1, "metered once: {records:?}");
    assert_eq!(records[0].tokens_in, 100);
    assert_eq!(records[0].tokens_out, 40);

    // A new call with the same token is refused.
    let again = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", JSON)
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 410);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.sink.records().len(), 1, "the refused call is not metered");
}
