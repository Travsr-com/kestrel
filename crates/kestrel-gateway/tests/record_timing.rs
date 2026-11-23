//! Issue #118: a call's meter record is stored before the client can see the end of the answer.
//!
//! An agent that calls one at a time reads the whole answer, then sends its next call. If the
//! record of the first call were still on its way, the budget check of the second would see a
//! stale total. The sink here takes a while to store a record, so a gateway that writes the
//! record after the body ends fails every time, not only on a loaded machine.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use bytes::Bytes;
use common::*;
use futures::StreamExt;
use kestrel_gateway::{BudgetGuard, GatewayDeps, MeterSink, router};
use kestrel_types::{ModelCallRecord, RunId};
use tokio::sync::Notify;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// How long the slow sink takes to store a record.
const STORE_DELAY: Duration = Duration::from_millis(200);

/// A meter sink that stores each record only after `delay`, as a busy database would.
struct SlowSink {
    delay: Duration,
    records: Mutex<Vec<ModelCallRecord>>,
}

impl SlowSink {
    fn records(&self) -> Vec<ModelCallRecord> {
        self.records.lock().unwrap().clone()
    }
}

#[async_trait]
impl MeterSink for SlowSink {
    async fn record(&self, rec: ModelCallRecord) {
        tokio::time::sleep(self.delay).await;
        self.records.lock().unwrap().push(rec);
    }
}

/// A budget with room for exactly one call: allowed while no call has been stored.
struct OneCall {
    sink: Arc<SlowSink>,
}

#[async_trait]
impl BudgetGuard for OneCall {
    async fn allow(&self, _run: RunId) -> bool {
        self.sink.records().is_empty()
    }
}

/// Serves the gateway with a one-call budget and a sink that stores after `delay`.
async fn gateway(upstream: &str, delay: Duration) -> (String, Arc<SlowSink>) {
    let sink = Arc::new(SlowSink {
        delay,
        records: Mutex::new(Vec::new()),
    });
    let deps = GatewayDeps {
        resolver: Arc::new(FakeResolver::with(RUN_TOKEN, RunId::new())),
        budget: Arc::new(OneCall { sink: sink.clone() }),
        meter: sink.clone(),
        tools: Arc::new(kestrel_gateway::NoTools),
        travsr: None,
    };
    let app = router(config(upstream, upstream), deps);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/r/{RUN_TOKEN}"), sink)
}

/// The four kinds of call: each provider, streamed or not.
#[derive(Clone, Copy, Debug)]
enum Kind {
    AnthropicJson,
    AnthropicSse,
    OpenAiJson,
    OpenAiSse,
}

const KINDS: [Kind; 4] = [
    Kind::AnthropicJson,
    Kind::AnthropicSse,
    Kind::OpenAiJson,
    Kind::OpenAiSse,
];

impl Kind {
    fn route(self) -> &'static str {
        match self {
            Kind::AnthropicJson | Kind::AnthropicSse => "/v1/messages",
            Kind::OpenAiJson | Kind::OpenAiSse => "/v1/chat/completions",
        }
    }

    fn request(self) -> String {
        match self {
            Kind::AnthropicJson => anthropic_request(CLAUDE_MODEL, false),
            Kind::AnthropicSse => anthropic_request(CLAUDE_MODEL, true),
            Kind::OpenAiJson => openai_request(OPENAI_MODEL, false),
            Kind::OpenAiSse => openai_request(OPENAI_MODEL, true),
        }
    }

    /// The events of a streamed answer, or the one body of a JSON answer.
    fn answer(self) -> Vec<String> {
        match self {
            Kind::AnthropicJson => vec![anthropic_response(CLAUDE_MODEL, 1200, 350, 0, 0)],
            Kind::AnthropicSse => anthropic_sse_events(CLAUDE_MODEL, 1200, 350, 0, 0),
            Kind::OpenAiJson => vec![openai_response(OPENAI_MODEL, 1200, 350, 0)],
            Kind::OpenAiSse => openai_sse_events(OPENAI_MODEL, 1200, 350, 0),
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Kind::AnthropicJson | Kind::OpenAiJson => "application/json",
            Kind::AnthropicSse | Kind::OpenAiSse => "text/event-stream",
        }
    }
}

/// An upstream that answers every call of `kind` at once.
async fn upstream(kind: Kind) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(kind.route()))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(kind.answer().concat(), kind.content_type()),
        )
        .mount(&server)
        .await;
    server
}

/// Sends one call and reads the whole answer. Returns the status and the body.
async fn call(client: &reqwest::Client, base: &str, kind: Kind) -> (u16, String) {
    let resp = client
        .post(format!("{base}{}", kind.route()))
        .body(kind.request())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

#[tokio::test]
async fn s1_a_budget_of_one_call_refuses_the_next_call_at_once_however_slow_the_store() {
    let client = reqwest::Client::new();
    for kind in KINDS {
        let server = upstream(kind).await;
        let (base, sink) = gateway(&server.uri(), STORE_DELAY).await;

        let (first, body) = call(&client, &base, kind).await;
        assert_eq!(first, 200, "{kind:?}: {body}");
        assert_eq!(
            body,
            kind.answer().concat(),
            "{kind:?}: the answer is unchanged"
        );
        // No pause: the next call goes out the moment the answer has been read.
        let (second, body) = call(&client, &base, kind).await;
        assert_eq!(second, 429, "{kind:?}: the second call got through: {body}");
        assert_eq!(sink.records().len(), 1, "{kind:?}");
    }
}

#[tokio::test]
async fn s1_repeated_200_times_with_a_store_that_takes_no_time() {
    let client = reqwest::Client::new();
    let server = upstream(Kind::AnthropicJson).await;
    for round in 0..200 {
        let (base, _sink) = gateway(&server.uri(), Duration::ZERO).await;
        let (first, _) = call(&client, &base, Kind::AnthropicJson).await;
        assert_eq!(first, 200, "round {round}");
        let (second, body) = call(&client, &base, Kind::AnthropicJson).await;
        assert_eq!(
            second, 429,
            "round {round}: the second call got through: {body}"
        );
    }
}

/// The text a streamed answer ends with: Anthropic's `message_stop` event, OpenAI's `[DONE]`.
fn final_event(kind: Kind) -> &'static str {
    match kind {
        Kind::AnthropicSse => "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        Kind::OpenAiSse => "data: [DONE]\n\n",
        Kind::AnthropicJson | Kind::OpenAiJson => unreachable!("only streamed answers"),
    }
}

#[tokio::test]
async fn s2_a_streaming_client_that_has_the_final_event_finds_the_record_stored() {
    let client = reqwest::Client::new();
    for kind in [Kind::AnthropicSse, Kind::OpenAiSse] {
        let server = upstream(kind).await;
        let (base, sink) = gateway(&server.uri(), STORE_DELAY).await;
        let resp = client
            .post(format!("{base}{}", kind.route()))
            .body(kind.request())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{kind:?}");

        // Read only until the final event is complete, as a client that stops there would.
        let mut stream = resp.bytes_stream();
        let mut got = Vec::new();
        while !String::from_utf8_lossy(&got).ends_with(final_event(kind)) {
            let chunk = stream
                .next()
                .await
                .unwrap_or_else(|| panic!("{kind:?}: the stream ended early"))
                .unwrap();
            got.extend_from_slice(&chunk);
        }
        assert_eq!(
            sink.records().len(),
            1,
            "{kind:?}: the client had the final event before the record was stored"
        );
    }
}

/// An upstream that streams the Anthropic events one by one: the first at once, the rest only
/// once `go` is notified. Returns its base URL.
async fn upstream_waiting_for(go: Arc<Notify>) -> String {
    let events = anthropic_sse_events(CLAUDE_MODEL, 1200, 350, 0, 0);
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let (events, go) = (events.clone(), go.clone());
            async move {
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(16);
                tokio::spawn(async move {
                    let mut events = events.into_iter();
                    if let Some(first) = events.next() {
                        let _ = tx.send(Ok(Bytes::from(first))).await;
                    }
                    go.notified().await;
                    for event in events {
                        let _ = tx.send(Ok(Bytes::from(event))).await;
                    }
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

#[tokio::test]
async fn s4_a_streamed_event_reaches_the_client_before_the_upstream_sends_the_next() {
    let go = Arc::new(Notify::new());
    let up = upstream_waiting_for(go.clone()).await;
    let (base, sink) = gateway(&up, STORE_DELAY).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .body(anthropic_request(CLAUDE_MODEL, true))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let events = anthropic_sse_events(CLAUDE_MODEL, 1200, 350, 0, 0);

    // The first event arrives while the upstream holds the rest back: nothing waits on it.
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while got.len() < events[0].len() {
            got.extend_from_slice(&stream.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("the first event was held back until the upstream sent more");
    assert_eq!(String::from_utf8_lossy(&got), events[0]);

    go.notify_one();
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(String::from_utf8_lossy(&got), events.concat());
    assert_eq!(sink.records().len(), 1);
    assert_eq!(sink.records()[0].tokens_out, 350);
}
