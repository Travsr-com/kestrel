//! Hardening tests from the issue #4 review: stalled upstreams, the request header
//! allowlist, pricing by the response model, and reading the body only after auth.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use futures::StreamExt;
use kestrel_gateway::{GatewayDeps, GatewayTimeouts, router, router_with_timeouts};
use kestrel_types::{MicroUsd, RunId};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Upstream that sends Anthropic `message_start` (25 input tokens, 1 output token) and then
/// never sends anything else, while keeping the connection open.
async fn start_stalling_upstream() -> String {
    let first = anthropic_sse_events(CLAUDE_MODEL, 25, 15, 0, 0).remove(0);
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let first = first.clone();
            async move {
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(1);
                tokio::spawn(async move {
                    let _ = tx.send(Ok(Bytes::from(first))).await;
                    // Hold the sender so the body never ends.
                    std::future::pending::<()>().await;
                    drop(tx);
                });
                axum::response::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(
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

/// Serve a router on an ephemeral port and return its address.
async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn deps() -> (RunId, Arc<FakeSink>, GatewayDeps) {
    let run = RunId::new();
    let sink = Arc::new(FakeSink::default());
    let deps = GatewayDeps {
        resolver: Arc::new(FakeResolver::with(RUN_TOKEN, run)),
        budget: Arc::new(FakeBudget::new(true)),
        meter: sink.clone(),
        tools: Arc::new(kestrel_gateway::NoTools),
        travsr: None,
    };
    (run, sink, deps)
}

#[test]
fn default_timeouts_are_10s_connect_and_300s_idle() {
    let t = GatewayTimeouts::default();
    assert_eq!(t.connect, Duration::from_secs(10));
    assert_eq!(t.idle, Duration::from_secs(300));
}

/// Upstream sends `message_start` then stalls; the client reads the first chunk and goes
/// away. Exactly one record is written, promptly, with the usage seen so far.
#[tokio::test]
async fn stalled_upstream_and_client_drop_writes_one_partial_record() {
    let base = start_stalling_upstream().await;
    let h = start(&base, &base, true).await;

    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", "application/json")
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    drop(stream);

    let recs = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let recs = h.sink.records();
            if !recs.is_empty() {
                return recs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no record within 3s of the client going away");
    // Give a second, wrong record a chance to show up before checking the count.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.sink.records().len(), 1, "{recs:?}");
    let rec = &recs[0];
    assert_eq!(rec.status, 200);
    assert_eq!(rec.model, CLAUDE_MODEL);
    assert_eq!((rec.tokens_in, rec.tokens_out), (25, 1));
    assert_eq!(rec.cost, Some(MicroUsd(25 * 3 + 15)));
}

/// Upstream stalls past the configured idle timeout while the client keeps waiting: the
/// gateway cuts the stream and records the partial usage.
#[tokio::test]
async fn upstream_idle_timeout_cuts_stream_and_records_partial_usage() {
    let base = start_stalling_upstream().await;
    let (run, sink, deps) = deps();
    let timeouts = GatewayTimeouts {
        connect: Duration::from_secs(1),
        idle: Duration::from_millis(200),
    };
    let gw = serve(router_with_timeouts(config(&base, &base), deps, timeouts)).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/r/{RUN_TOKEN}/v1/messages"))
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = tokio::time::timeout(Duration::from_secs(3), resp.bytes())
        .await
        .expect("idle timeout did not end the client stream");
    assert!(body.is_err(), "a cut stream is an error for the client");

    let recs = sink.wait_for(1).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].run_id, run);
    assert_eq!((recs[0].tokens_in, recs[0].tokens_out), (25, 1));
}

/// Only allowlisted headers go upstream: account routing headers and cookies are dropped.
#[tokio::test]
async fn only_allowlisted_request_headers_reach_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            openai_response(OPENAI_MODEL, 10, 2, 0).into_bytes(),
            "application/json",
        ))
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .header("content-type", "application/json")
        .header("user-agent", "agent-cli/1.0")
        .header("anthropic-beta", "prompt-caching-2024-07-31")
        .header("openai-organization", "org-client")
        .header("openai-project", "proj-client")
        .header("cookie", "session=abc")
        .header("proxy-authorization", "Basic abc")
        .header("x-custom", "1")
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let reqs = upstream.received_requests().await.unwrap();
    let hdrs = &reqs[0].headers;
    for gone in [
        "openai-organization",
        "openai-project",
        "cookie",
        "proxy-authorization",
        "x-custom",
        "x-api-key",
    ] {
        assert!(hdrs.get(gone).is_none(), "{gone} must not reach upstream");
    }
    assert_eq!(hdrs.get("user-agent").unwrap(), "agent-cli/1.0");
    assert_eq!(
        hdrs.get("anthropic-beta").unwrap(),
        "prompt-caching-2024-07-31"
    );
    assert_eq!(
        hdrs.get("authorization").unwrap().to_str().unwrap(),
        format!("Bearer {OPENAI_KEY}")
    );
}

/// The request names the model twice (the unpriced one last, which a JSON parser keeps),
/// but the provider answers for a priced model. Cost follows the response.
#[tokio::test]
async fn cost_uses_response_model_not_request_model() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            anthropic_response(CLAUDE_MODEL, 1200, 350, 8000, 2000).into_bytes(),
            "application/json",
        ))
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let body = format!(
        r#"{{"model":"{CLAUDE_MODEL}","model":"{UNPRICED_MODEL}","max_tokens":8,"messages":[{{"role":"user","content":"hi"}}]}}"#
    );
    let resp = h
        .client
        .post(h.anthropic_url(RUN_TOKEN))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();

    let recs = h.sink.wait_for(1).await;
    assert_eq!(recs[0].model, CLAUDE_MODEL);
    assert_eq!(recs[0].cost, Some(MicroUsd(18_750)));
}

/// Same rule for an OpenAI stream: the model comes from the chunks.
#[tokio::test]
async fn openai_stream_cost_uses_chunk_model() {
    let upstream = MockServer::start().await;
    let events = openai_sse_events(OPENAI_MODEL, 2_000_000, 1_000_000, 1_000_000);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(events.concat().into_bytes(), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .body(openai_request(UNPRICED_MODEL, true))
        .send()
        .await
        .unwrap();
    let _ = resp.bytes().await.unwrap();

    let recs = h.sink.wait_for(1).await;
    assert_eq!(recs[0].model, OPENAI_MODEL);
    assert_eq!(recs[0].cost, Some(MicroUsd(825_000)));
}

/// An unknown run token is rejected before the body is read: the body stream is never polled.
#[tokio::test]
async fn unknown_token_rejected_before_body_is_read() {
    let (_, sink, deps) = deps();
    let app = router(config("http://127.0.0.1:9", "http://127.0.0.1:9"), deps);
    let polled = Arc::new(AtomicBool::new(false));
    let flag = polled.clone();
    let body = futures::stream::once(async move {
        flag.store(true, Ordering::SeqCst);
        Ok::<_, Infallible>(Bytes::from(vec![b' '; 40 * 1024 * 1024]))
    });
    let req = Request::post("/r/rt_unknown/v1/messages")
        .header("content-length", (40 * 1024 * 1024).to_string())
        .body(Body::from_stream(body))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 401);
    assert!(!polled.load(Ordering::SeqCst), "body was read before auth");
    assert!(sink.records().is_empty());
}

/// A known caller sending more than 32 MiB gets 413 in the provider's error format.
#[tokio::test]
async fn oversized_body_is_413_in_provider_format() {
    for (route, check) in [
        (
            "v1/messages",
            (|b: &serde_json::Value| assert_anthropic_error(b, "request_too_large"))
                as fn(&serde_json::Value),
        ),
        ("v1/chat/completions", |b: &serde_json::Value| {
            assert_openai_error(b, "invalid_request_error")
        }),
    ] {
        let (_, sink, deps) = deps();
        let app = router(config("http://127.0.0.1:9", "http://127.0.0.1:9"), deps);
        let req = Request::post(format!("/r/{RUN_TOKEN}/{route}"))
            .body(Body::from(vec![b' '; 32 * 1024 * 1024 + 1]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 413);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        check(&serde_json::from_slice(&bytes).unwrap());
        assert!(sink.records().is_empty());
    }
}

/// The provider answers with a dated snapshot of a priced model: it is priced via the
/// longest matching price-table prefix, and the record keeps the snapshot name.
#[tokio::test]
async fn dated_snapshot_model_priced_by_prefix() {
    let upstream = MockServer::start().await;
    let snapshot = format!("{OPENAI_MODEL}-2024-07-18");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            openai_response(&snapshot, 2_000_000, 1_000_000, 1_000_000).into_bytes(),
            "application/json",
        ))
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let resp = h
        .client
        .post(h.openai_url(RUN_TOKEN))
        .body(openai_request(OPENAI_MODEL, false))
        .send()
        .await
        .unwrap();
    let _ = resp.bytes().await.unwrap();

    let recs = h.sink.wait_for(1).await;
    assert_eq!(recs[0].model, snapshot);
    assert_eq!(recs[0].cost, Some(MicroUsd(825_000)));
}

/// A long non-streaming call: the upstream waits before sending headers, and the client
/// gives up first. The provider still bills the call, so the gateway lets it finish and
/// writes exactly one record with the real usage.
#[tokio::test]
async fn client_leaving_before_headers_still_meters_the_call() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    anthropic_response(CLAUDE_MODEL, 1200, 350, 8000, 2000).into_bytes(),
                    "application/json",
                )
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&upstream)
        .await;
    let h = start(&upstream.uri(), &upstream.uri(), true).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let sent = client
        .post(h.anthropic_url(RUN_TOKEN))
        .body(anthropic_request(CLAUDE_MODEL, false))
        .send()
        .await;
    assert!(sent.is_err(), "client must give up before headers arrive");
    drop(client);

    let recs = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let recs = h.sink.records();
            if !recs.is_empty() {
                return recs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no record within 3s although the upstream answered");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.sink.records().len(), 1, "{recs:?}");
    let rec = &recs[0];
    assert_eq!(rec.status, 200);
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
}
