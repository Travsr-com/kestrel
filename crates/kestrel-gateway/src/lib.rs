//! LLM gateway: Anthropic and OpenAI compatible proxy with cost metering.
//!
//! Routes (run token in the path, because agent CLIs let you set a base URL but not headers):
//!
//! * `POST /r/{run_token}/v1/messages` forwards to `{anthropic.base_url}/v1/messages`.
//! * `POST /r/{run_token}/v1/chat/completions` forwards to `{openai.base_url}/v1/chat/completions`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures::StreamExt;
use kestrel_types::{MicroUsd, ModelCallRecord, RunId};
use serde_json::{Value, json};
use url::Url;

/// Maps the run token from the URL path to a run.
#[async_trait]
pub trait RunResolver: Send + Sync {
    async fn resolve(&self, token: &str) -> Option<RunId>;
}

/// Decides whether a run may make another model call.
#[async_trait]
pub trait BudgetGuard: Send + Sync {
    async fn allow(&self, run: RunId) -> bool;
}

/// Receives one record per model call.
#[async_trait]
pub trait MeterSink: Send + Sync {
    async fn record(&self, rec: ModelCallRecord);
}

/// One upstream provider: where to send requests and which key to use.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Origin of the provider API, without the `/v1/...` path, e.g. `https://api.anthropic.com`.
    pub base_url: Url,
    /// The provider API key. Any key the client sends is dropped in favour of this one.
    pub api_key: String,
}

/// Prices for one model, in `MicroUsd` per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPrice {
    pub input_per_mtok: MicroUsd,
    pub output_per_mtok: MicroUsd,
    pub cache_read_per_mtok: MicroUsd,
    pub cache_write_per_mtok: MicroUsd,
}

/// Gateway configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub anthropic: ProviderConfig,
    pub openai: ProviderConfig,
    /// Price table keyed by model name. A model missing here gives `cost: None`.
    pub prices: HashMap<String, ModelPrice>,
}

/// Collaborators the gateway calls out to.
#[derive(Clone)]
pub struct GatewayDeps {
    pub resolver: Arc<dyn RunResolver>,
    pub budget: Arc<dyn BudgetGuard>,
    pub meter: Arc<dyn MeterSink>,
}

/// Largest request body the gateway accepts (LLM prompts can be large).
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

/// Request headers never forwarded upstream: client credentials and hop-by-hop headers.
/// `accept-encoding` is dropped so the upstream body stays readable by the usage parser.
const DROP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "x-api-key",
    "authorization",
    "proxy-authorization",
    "accept-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Response headers never copied back to the client (the body is re-framed by the server).
const DROP_RESPONSE_HEADERS: &[&str] = &[
    "content-length",
    "connection",
    "keep-alive",
    "transfer-encoding",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Anthropic,
    OpenAi,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAi => "openai",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Provider::Anthropic => "/v1/messages",
            Provider::OpenAi => "/v1/chat/completions",
        }
    }

    /// An error response in this provider's JSON shape.
    fn error(self, status: StatusCode, message: &str) -> Response {
        let body = match self {
            Provider::Anthropic => {
                let kind = match status {
                    StatusCode::UNAUTHORIZED => "authentication_error",
                    StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
                    _ => "api_error",
                };
                json!({"type": "error", "error": {"type": kind, "message": message}})
            }
            Provider::OpenAi => {
                let kind = match status {
                    StatusCode::UNAUTHORIZED => "invalid_request_error",
                    StatusCode::TOO_MANY_REQUESTS => "insufficient_quota",
                    _ => "server_error",
                };
                json!({"error": {"message": message, "type": kind, "param": null, "code": kind}})
            }
        };
        let mut resp = Response::new(Body::from(body.to_string()));
        *resp.status_mut() = status;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        resp
    }
}

struct AppState {
    cfg: GatewayConfig,
    deps: GatewayDeps,
    http: reqwest::Client,
}

impl AppState {
    fn provider(&self, p: Provider) -> &ProviderConfig {
        match p {
            Provider::Anthropic => &self.cfg.anthropic,
            Provider::OpenAi => &self.cfg.openai,
        }
    }
}

/// Build the gateway router.
pub fn router(cfg: GatewayConfig, deps: GatewayDeps) -> axum::Router {
    let state = Arc::new(AppState {
        cfg,
        deps,
        http: reqwest::Client::new(),
    });
    axum::Router::new()
        .route(
            "/r/{run_token}/v1/messages",
            post(
                |State(s): State<Arc<AppState>>,
                 Path(token): Path<String>,
                 headers: HeaderMap,
                 body: Bytes| async move {
                    proxy(s, Provider::Anthropic, token, headers, body).await
                },
            ),
        )
        .route(
            "/r/{run_token}/v1/chat/completions",
            post(
                |State(s): State<Arc<AppState>>,
                 Path(token): Path<String>,
                 headers: HeaderMap,
                 body: Bytes| async move {
                    proxy(s, Provider::OpenAi, token, headers, body).await
                },
            ),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

/// Facts about one call, gathered before the upstream answers.
struct Call {
    state: Arc<AppState>,
    provider: Provider,
    run: RunId,
    model: String,
    started: Instant,
}

impl Call {
    async fn record(self, status: u16, usage: Option<Usage>) {
        let usage_or_zero = usage.unwrap_or_default();
        let cost = match usage {
            Some(u) => self
                .state
                .cfg
                .prices
                .get(&self.model)
                .and_then(|p| cost(&u, p)),
            None => Some(MicroUsd(0)),
        };
        let rec = ModelCallRecord {
            run_id: self.run,
            provider: self.provider.name().to_string(),
            model: self.model,
            tokens_in: usage_or_zero.input,
            tokens_out: usage_or_zero.output,
            tokens_cache_read: usage_or_zero.cache_read,
            tokens_cache_write: usage_or_zero.cache_write,
            cost,
            latency_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            status,
        };
        self.state.deps.meter.record(rec).await;
    }
}

async fn proxy(
    state: Arc<AppState>,
    provider: Provider,
    token: String,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let Some(run) = state.deps.resolver.resolve(&token).await else {
        return provider.error(StatusCode::UNAUTHORIZED, "unknown run token");
    };
    if !state.deps.budget.allow(run).await {
        return provider.error(
            StatusCode::TOO_MANY_REQUESTS,
            "the run budget is used up, no further model calls are allowed",
        );
    }

    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    let upstream = state.provider(provider);
    let url = format!(
        "{}{}",
        upstream.base_url.as_str().trim_end_matches('/'),
        provider.path()
    );

    let mut out_headers = HeaderMap::new();
    for (name, value) in &headers {
        if !DROP_REQUEST_HEADERS.contains(&name.as_str()) {
            out_headers.append(name.clone(), value.clone());
        }
    }
    let key = match provider {
        Provider::Anthropic => ("x-api-key", upstream.api_key.clone()),
        Provider::OpenAi => ("authorization", format!("Bearer {}", upstream.api_key)),
    };
    let call = Call {
        state: state.clone(),
        provider,
        run,
        model,
        started,
    };
    let Ok(key_value) = HeaderValue::from_str(&key.1) else {
        call.record(StatusCode::BAD_GATEWAY.as_u16(), None).await;
        return provider.error(
            StatusCode::BAD_GATEWAY,
            "gateway API key is not a valid header",
        );
    };
    out_headers.insert(key.0, key_value);

    let sent = state
        .http
        .post(url)
        .headers(out_headers)
        .body(body)
        .send()
        .await;
    let upstream_resp = match sent {
        Ok(r) => r,
        Err(_) => {
            call.record(StatusCode::BAD_GATEWAY.as_u16(), None).await;
            return provider.error(StatusCode::BAD_GATEWAY, "upstream provider is unreachable");
        }
    };

    let status = upstream_resp.status();
    let mut resp_headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers() {
        if !DROP_RESPONSE_HEADERS.contains(&name.as_str()) {
            resp_headers.append(name.clone(), value.clone());
        }
    }
    let is_sse = resp_headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));

    // Pipe the upstream body to the client chunk by chunk, teeing each chunk to the usage
    // parser. The record is written once the body ends (or the client goes away).
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, reqwest::Error>>(16);
    let mut upstream_body = upstream_resp.bytes_stream();
    tokio::spawn(async move {
        let mut parser = UsageParser::new(provider, is_sse);
        while let Some(item) = upstream_body.next().await {
            let failed = item.is_err();
            if let Ok(chunk) = &item {
                parser.feed(chunk);
            }
            if tx.send(item).await.is_err() || failed {
                break;
            }
        }
        drop(tx);
        // A failed call is not billed: zero tokens, zero cost.
        let usage = status.is_success().then(|| parser.finish());
        call.record(status.as_u16(), usage).await;
    });

    let mut resp = Response::new(Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(rx),
    ));
    *resp.status_mut() = status;
    *resp.headers_mut() = resp_headers;
    resp
}

/// Token counts for one call. `input` excludes cache reads and writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

/// Reads usage out of a response body as it streams past.
struct UsageParser {
    provider: Provider,
    sse: bool,
    buf: Vec<u8>,
    usage: Usage,
}

impl UsageParser {
    fn new(provider: Provider, sse: bool) -> Self {
        Self {
            provider,
            sse,
            buf: Vec::new(),
            usage: Usage::default(),
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        if self.sse {
            while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                self.sse_line(&line);
            }
        }
    }

    fn finish(mut self) -> Usage {
        let rest = std::mem::take(&mut self.buf);
        if self.sse {
            self.sse_line(&rest);
        } else if let Ok(v) = serde_json::from_slice::<Value>(&rest) {
            self.apply(&v);
        }
        self.usage
    }

    fn sse_line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        if let Ok(v) = serde_json::from_slice::<Value>(data) {
            self.apply(&v);
        }
    }

    /// Apply the usage found in one JSON document. Counts are cumulative, so they replace.
    fn apply(&mut self, v: &Value) {
        let set = |slot: &mut u64, u: &Value, key: &str| {
            if let Some(n) = u.get(key).and_then(Value::as_u64) {
                *slot = n;
            }
        };
        match self.provider {
            Provider::Anthropic => {
                let Some(u) = v
                    .get("usage")
                    .filter(|u| u.is_object())
                    .or_else(|| v.pointer("/message/usage"))
                else {
                    return;
                };
                set(&mut self.usage.input, u, "input_tokens");
                set(&mut self.usage.output, u, "output_tokens");
                set(&mut self.usage.cache_read, u, "cache_read_input_tokens");
                set(
                    &mut self.usage.cache_write,
                    u,
                    "cache_creation_input_tokens",
                );
            }
            Provider::OpenAi => {
                let Some(u) = v.get("usage").filter(|u| u.is_object()) else {
                    return;
                };
                if let Some(prompt) = u.get("prompt_tokens").and_then(Value::as_u64) {
                    let cached = u
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.usage.input = prompt.saturating_sub(cached);
                    self.usage.cache_read = cached;
                }
                set(&mut self.usage.output, u, "completion_tokens");
            }
        }
    }
}

/// Cost of `u` at price `p`, rounding any fractional micro-dollar up so budgets never
/// under-count. `None` only if the result does not fit in `MicroUsd`.
fn cost(u: &Usage, p: &ModelPrice) -> Option<MicroUsd> {
    const MTOK: i128 = 1_000_000;
    let mut total: i128 = 0;
    for (tokens, price) in [
        (u.input, p.input_per_mtok),
        (u.output, p.output_per_mtok),
        (u.cache_read, p.cache_read_per_mtok),
        (u.cache_write, p.cache_write_per_mtok),
    ] {
        total = total.checked_add(i128::from(tokens).checked_mul(i128::from(price.0))?)?;
    }
    let mut micro = total.div_euclid(MTOK);
    if total.rem_euclid(MTOK) != 0 {
        micro += 1;
    }
    i64::try_from(micro).ok().map(MicroUsd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(input: i64, output: i64, cache_read: i64, cache_write: i64) -> ModelPrice {
        ModelPrice {
            input_per_mtok: MicroUsd(input),
            output_per_mtok: MicroUsd(output),
            cache_read_per_mtok: MicroUsd(cache_read),
            cache_write_per_mtok: MicroUsd(cache_write),
        }
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    #[test]
    fn cost_whole_micro_dollars_are_exact() {
        // 1 input token at $3 per million tokens is exactly 3 micro-dollars.
        let p = price(3_000_000, 0, 0, 0);
        assert_eq!(cost(&usage(1, 0, 0, 0), &p), Some(MicroUsd(3)));
        assert_eq!(cost(&usage(0, 0, 0, 0), &p), Some(MicroUsd(0)));
    }

    #[test]
    fn cost_rounds_fractional_micro_dollars_up() {
        // 1 token at $0.10 per million tokens is 0.1 micro-dollars, billed as 1.
        let p = price(100_000, 100_000, 100_000, 100_000);
        assert_eq!(cost(&usage(1, 0, 0, 0), &p), Some(MicroUsd(1)));
        assert_eq!(cost(&usage(0, 0, 1, 0), &p), Some(MicroUsd(1)));
        // 10 tokens at $0.10 is exactly 1, 11 tokens is 1.1 and rounds to 2.
        assert_eq!(cost(&usage(10, 0, 0, 0), &p), Some(MicroUsd(1)));
        assert_eq!(cost(&usage(5, 6, 0, 0), &p), Some(MicroUsd(2)));
    }

    #[test]
    fn cost_too_large_for_micro_usd_is_none() {
        let p = price(i64::MAX, i64::MAX, i64::MAX, i64::MAX);
        assert_eq!(cost(&usage(u64::MAX, 0, 0, 0), &p), None);
        assert_eq!(
            cost(&usage(u64::MAX, u64::MAX, u64::MAX, u64::MAX), &p),
            None
        );
    }

    #[test]
    fn sse_parser_handles_crlf_split_chunks_and_trailing_line() {
        let mut p = UsageParser::new(Provider::Anthropic, true);
        p.feed(
            b"event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":",
        );
        p.feed(b"{\"input_tokens\":7,\"output_tokens\":1}}}\r\n\r\n");
        p.feed(b"data: not json\n\n");
        p.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}");
        assert_eq!(p.finish(), usage(7, 9, 0, 0));
    }

    #[test]
    fn json_parser_ignores_bodies_without_usage() {
        let mut p = UsageParser::new(Provider::OpenAi, false);
        p.feed(br#"{"id":"x","usage":null}"#);
        assert_eq!(p.finish(), Usage::default());
        let mut p = UsageParser::new(Provider::Anthropic, false);
        p.feed(b"not json");
        assert_eq!(p.finish(), Usage::default());
    }
}
