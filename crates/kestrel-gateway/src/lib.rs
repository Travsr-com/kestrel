//! LLM gateway: Anthropic and OpenAI compatible proxy with cost metering.
//!
//! Routes (run token in the path, because agent CLIs let you set a base URL but not headers):
//!
//! * `POST /r/{run_token}/v1/messages` forwards to `{anthropic.base_url}/v1/messages`.
//! * `POST /r/{run_token}/v1/chat/completions` forwards to `{openai.base_url}/v1/chat/completions`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use kestrel_types::{MicroUsd, ModelCallRecord, RunId};
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

/// Build the gateway router.
pub fn router(cfg: GatewayConfig, deps: GatewayDeps) -> axum::Router {
    let _ = (cfg, deps);
    todo!("kestrel-gateway router, see issue #4")
}
