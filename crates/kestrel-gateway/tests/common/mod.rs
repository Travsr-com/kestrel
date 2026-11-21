//! Shared harness for the gateway scenario tests: fakes, config and a served gateway.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kestrel_gateway::{
    BudgetGuard, GatewayConfig, GatewayDeps, MeterSink, ModelPrice, NoTools, ProviderConfig,
    RunResolver, router,
};
use kestrel_types::{MicroUsd, ModelCallRecord, RunId};
use url::Url;

pub const RUN_TOKEN: &str = "rt_live_7f3a9c";
pub const ANTHROPIC_KEY: &str = "sk-ant-configured-key";
pub const OPENAI_KEY: &str = "sk-openai-configured-key";

pub const CLAUDE_MODEL: &str = "claude-sonnet-4-5";
pub const OPENAI_MODEL: &str = "gpt-4o-mini";
pub const UNPRICED_MODEL: &str = "claude-not-in-price-table";

/// Fake `RunResolver`: knows a fixed set of tokens and records every lookup.
pub struct FakeResolver {
    known: HashMap<String, RunId>,
    pub calls: Mutex<Vec<String>>,
}

impl FakeResolver {
    pub fn with(token: &str, run: RunId) -> Self {
        let mut known = HashMap::new();
        known.insert(token.to_string(), run);
        Self {
            known,
            calls: Mutex::new(Vec::new()),
        }
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl RunResolver for FakeResolver {
    async fn resolve(&self, token: &str) -> Option<RunId> {
        self.calls.lock().unwrap().push(token.to_string());
        self.known.get(token).copied()
    }
}

/// Fake `BudgetGuard`: answers a fixed value and records every run it was asked about.
pub struct FakeBudget {
    allow: bool,
    pub calls: Mutex<Vec<RunId>>,
}

impl FakeBudget {
    pub fn new(allow: bool) -> Self {
        Self {
            allow,
            calls: Mutex::new(Vec::new()),
        }
    }
    pub fn calls(&self) -> Vec<RunId> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl BudgetGuard for FakeBudget {
    async fn allow(&self, run: RunId) -> bool {
        self.calls.lock().unwrap().push(run);
        self.allow
    }
}

/// Fake `MeterSink`: keeps every record.
#[derive(Default)]
pub struct FakeSink {
    pub records: Mutex<Vec<ModelCallRecord>>,
}

impl FakeSink {
    pub fn records(&self) -> Vec<ModelCallRecord> {
        self.records.lock().unwrap().clone()
    }

    /// Records may be written after the response body ends, so poll briefly.
    pub async fn wait_for(&self, n: usize) -> Vec<ModelCallRecord> {
        for _ in 0..100 {
            let recs = self.records();
            if recs.len() >= n {
                return recs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.records()
    }
}

#[async_trait]
impl MeterSink for FakeSink {
    async fn record(&self, rec: ModelCallRecord) {
        self.records.lock().unwrap().push(rec);
    }
}

/// Everything a scenario needs to drive and inspect the gateway.
pub struct Harness {
    pub addr: SocketAddr,
    pub run: RunId,
    pub resolver: Arc<FakeResolver>,
    pub budget: Arc<FakeBudget>,
    pub sink: Arc<FakeSink>,
    pub client: reqwest::Client,
}

impl Harness {
    pub fn anthropic_url(&self, token: &str) -> String {
        format!("http://{}/r/{token}/v1/messages", self.addr)
    }
    pub fn openai_url(&self, token: &str) -> String {
        format!("http://{}/r/{token}/v1/chat/completions", self.addr)
    }
}

/// Price table used by every scenario, in `MicroUsd` per million tokens.
pub fn prices() -> HashMap<String, ModelPrice> {
    let mut p = HashMap::new();
    // $3 in, $15 out, $0.30 cache read, $3.75 cache write per million tokens.
    p.insert(
        CLAUDE_MODEL.to_string(),
        ModelPrice {
            input_per_mtok: MicroUsd(3_000_000),
            output_per_mtok: MicroUsd(15_000_000),
            cache_read_per_mtok: MicroUsd(300_000),
            cache_write_per_mtok: MicroUsd(3_750_000),
        },
    );
    // $0.15 in, $0.60 out, $0.075 cache read, no cache write per million tokens.
    p.insert(
        OPENAI_MODEL.to_string(),
        ModelPrice {
            input_per_mtok: MicroUsd(150_000),
            output_per_mtok: MicroUsd(600_000),
            cache_read_per_mtok: MicroUsd(75_000),
            cache_write_per_mtok: MicroUsd(0),
        },
    );
    p
}

pub fn config(anthropic_base: &str, openai_base: &str) -> GatewayConfig {
    GatewayConfig {
        anthropic: ProviderConfig {
            base_url: Url::parse(anthropic_base).unwrap(),
            api_key: ANTHROPIC_KEY.to_string(),
        },
        openai: ProviderConfig {
            base_url: Url::parse(openai_base).unwrap(),
            api_key: OPENAI_KEY.to_string(),
        },
        prices: prices(),
    }
}

/// Serve the gateway on an ephemeral local port with fresh fakes.
pub async fn start(anthropic_base: &str, openai_base: &str, allow_budget: bool) -> Harness {
    let run = RunId::new();
    let resolver = Arc::new(FakeResolver::with(RUN_TOKEN, run));
    let budget = Arc::new(FakeBudget::new(allow_budget));
    let sink = Arc::new(FakeSink::default());
    let deps = GatewayDeps {
        resolver: resolver.clone(),
        budget: budget.clone(),
        meter: sink.clone(),
        tools: Arc::new(NoTools),
    };
    let app = router(config(anthropic_base, openai_base), deps);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    Harness {
        addr,
        run,
        resolver,
        budget,
        sink,
        client,
    }
}

/// A base URL nothing listens on: bind an ephemeral port, then drop the listener.
/// A base URL where every connection is refused.
///
/// Port 1 is privileged, so no test process can bind it, and nothing listens there on CI
/// or dev machines. Binding port 0 and dropping the listener was racy: a test running in
/// parallel could grab the freed port and answer.
pub async fn dead_base_url() -> String {
    "http://127.0.0.1:1".to_string()
}

// ---------------------------------------------------------------------------
// Realistic provider payloads (shapes from the public Anthropic Messages API and
// OpenAI Chat Completions API docs).
// ---------------------------------------------------------------------------

/// Anthropic request body. Deliberately odd whitespace so a re-serialized body is caught.
pub fn anthropic_request(model: &str, stream: bool) -> String {
    format!(
        r#"{{"model": "{model}",  "max_tokens":1024,
 "stream":{stream},"system":[{{"type":"text","text":"You are a terse assistant.","cache_control":{{"type":"ephemeral"}}}}],
 "messages":[{{"role":"user","content":"Say hello."}}]}}"#
    )
}

/// Anthropic non-streaming response body, with cache tokens. Odd whitespace on purpose.
pub fn anthropic_response(model: &str, input: u64, output: u64, cr: u64, cw: u64) -> String {
    format!(
        r#"{{"id":"msg_01XFDUDYJgAACzvnptvVoYEL","type":"message","role":"assistant","model":"{model}",
  "content":[{{"type":"text","text":"Hello!"}}],"stop_reason":"end_turn","stop_sequence":null,
  "usage":{{"input_tokens":{input},"cache_creation_input_tokens":{cw},"cache_read_input_tokens":{cr},"output_tokens":{output}}}}}"#
    )
}

/// Anthropic SSE events. `message_start` carries input and cache tokens (and an
/// initial `output_tokens: 1`); `message_delta` carries the final cumulative `output_tokens`.
pub fn anthropic_sse_events(model: &str, input: u64, output: u64, cr: u64, cw: u64) -> Vec<String> {
    vec![
        format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_01HCDu5LRGeP2o7s2xGmxyx8\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"{model}\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":{input},\"cache_creation_input_tokens\":{cw},\"cache_read_input_tokens\":{cr},\"output_tokens\":1}}}}}}\n\n"
        ),
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_string(),
        "event: ping\ndata: {\"type\": \"ping\"}\n\n".to_string(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n".to_string(),
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"!\"}}\n\n".to_string(),
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_string(),
        format!(
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":{output}}}}}\n\n"
        ),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
    ]
}

/// OpenAI request body.
pub fn openai_request(model: &str, stream: bool) -> String {
    if stream {
        format!(
            r#"{{"model":"{model}","stream":true,"stream_options":{{"include_usage":true}},"messages":[{{"role":"system","content":"You are a terse assistant."}},{{"role":"user","content":"Say hello."}}]}}"#
        )
    } else {
        format!(
            r#"{{"model":"{model}","messages":[{{"role":"system","content":"You are a terse assistant."}},{{"role":"user","content":"Say hello."}}]}}"#
        )
    }
}

/// OpenAI non-streaming response. `prompt_tokens` includes `cached_tokens`.
pub fn openai_response(model: &str, prompt: u64, completion: u64, cached: u64) -> String {
    let total = prompt + completion;
    format!(
        r#"{{"id":"chatcmpl-B9MBs8CjcvOU2jLn4n570S5qMJKcT","object":"chat.completion","created":1741569952,"model":"{model}","choices":[{{"index":0,"message":{{"role":"assistant","content":"Hello!","refusal":null}},"logprobs":null,"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{prompt},"completion_tokens":{completion},"total_tokens":{total},"prompt_tokens_details":{{"cached_tokens":{cached},"audio_tokens":0}},"completion_tokens_details":{{"reasoning_tokens":0,"audio_tokens":0,"accepted_prediction_tokens":0,"rejected_prediction_tokens":0}}}},"system_fingerprint":"fp_06737a9306"}}"#
    )
}

/// OpenAI SSE chunks with `stream_options.include_usage`: every chunk has `usage: null`
/// except the last one, which has empty `choices` and the totals. Then `[DONE]`.
pub fn openai_sse_events(model: &str, prompt: u64, completion: u64, cached: u64) -> Vec<String> {
    let head = format!(
        "\"id\":\"chatcmpl-B9MHDbslfkBeAs8l4bebGdFOJ6PeG\",\"object\":\"chat.completion.chunk\",\"created\":1741570283,\"model\":\"{model}\",\"system_fingerprint\":\"fp_06737a9306\""
    );
    let total = prompt + completion;
    vec![
        format!(
            "data: {{{head},\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"\",\"refusal\":null}},\"logprobs\":null,\"finish_reason\":null}}],\"usage\":null}}\n\n"
        ),
        format!(
            "data: {{{head},\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"Hello\"}},\"logprobs\":null,\"finish_reason\":null}}],\"usage\":null}}\n\n"
        ),
        format!(
            "data: {{{head},\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"!\"}},\"logprobs\":null,\"finish_reason\":null}}],\"usage\":null}}\n\n"
        ),
        format!(
            "data: {{{head},\"choices\":[{{\"index\":0,\"delta\":{{}},\"logprobs\":null,\"finish_reason\":\"stop\"}}],\"usage\":null}}\n\n"
        ),
        format!(
            "data: {{{head},\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt},\"completion_tokens\":{completion},\"total_tokens\":{total},\"prompt_tokens_details\":{{\"cached_tokens\":{cached},\"audio_tokens\":0}},\"completion_tokens_details\":{{\"reasoning_tokens\":0,\"audio_tokens\":0,\"accepted_prediction_tokens\":0,\"rejected_prediction_tokens\":0}}}}}}\n\n"
        ),
        "data: [DONE]\n\n".to_string(),
    ]
}

/// Anthropic error body: `{"type":"error","error":{"type":"<kind>","message":"..."}}`.
pub fn assert_anthropic_error(body: &serde_json::Value, kind: &str) {
    assert_eq!(body["type"], "error", "anthropic error envelope: {body}");
    assert_eq!(body["error"]["type"], kind, "anthropic error type: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "anthropic error message: {body}"
    );
}

/// OpenAI error body: `{"error":{"message":"...","type":"<kind>","param":null,"code":...}}`.
pub fn assert_openai_error(body: &serde_json::Value, kind: &str) {
    assert!(body["error"].is_object(), "openai error envelope: {body}");
    assert_eq!(body["error"]["type"], kind, "openai error type: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "openai error message: {body}"
    );
}
