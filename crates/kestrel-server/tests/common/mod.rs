//! Shared fixtures for the server tests: a store, specs, the router with an in-memory UI, and
//! request helpers.

#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use kestrel_server::{Asset, Assets, Config, router};
use kestrel_store::Store;
use kestrel_types::{AgentSpec, RunId, RunStatus};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

pub const TOKEN: &str = "0123456789abcdef0123456789abcdef";
pub const HOST: &str = "127.0.0.1:7171";
pub const INDEX: &str = "<!doctype html><title>Kestrel</title>";

pub fn bind() -> SocketAddr {
    HOST.parse().unwrap()
}

/// An in-memory UI build: `index.html` and one script.
pub struct MemUi(pub HashMap<String, Asset>);

impl MemUi {
    pub fn build() -> MemUi {
        let mut files = HashMap::new();
        files.insert(
            "index.html".to_string(),
            Asset {
                body: INDEX.as_bytes().to_vec(),
                mime: "text/html".into(),
            },
        );
        files.insert(
            "assets/app.js".to_string(),
            Asset {
                body: b"console.log(1)".to_vec(),
                mime: "text/javascript".into(),
            },
        );
        MemUi(files)
    }

    pub fn empty() -> MemUi {
        MemUi(HashMap::new())
    }
}

impl Assets for MemUi {
    fn get(&self, path: &str) -> Option<Asset> {
        self.0.get(path).cloned()
    }
}

pub fn store() -> Arc<Store> {
    Arc::new(Store::open_in_memory().unwrap())
}

pub fn agent(name: &str) -> AgentSpec {
    AgentSpec::from_yaml(&format!(
        "name: {name}\nruntime: opencode\nrole: r\n\
         capabilities: {{ mcp: [kestrel.spawn], spawn: {{ max_children: 5, max_depth: 2, agents: [{name}] }} }}\n\
         sandbox: {{ tier: gvisor, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 2.00, wall_clock: 10m }}\n"
    ))
    .unwrap()
}

pub fn app_with(store: Arc<Store>, ui: MemUi) -> Router {
    router(
        store,
        Config::new(TOKEN, bind()),
        Arc::new(ui),
        CancellationToken::new(),
    )
}

pub fn app(store: Arc<Store>) -> Router {
    app_with(store, MemUi::build())
}

/// A request with the right Host and token.
pub fn req(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", HOST)
        .header("authorization", format!("Bearer {TOKEN}"));
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    b.body(body.map_or_else(Body::empty, |s| Body::from(s.to_string())))
        .unwrap()
}

pub fn get(uri: &str) -> Request<Body> {
    req("GET", uri, None)
}

pub async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let res = app.clone().oneshot(request).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, headers, body)
}

pub async fn json(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = send(app, request).await;
    let value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

/// Moves a queued run to `Running`.
pub fn run(store: &Store, id: RunId) {
    store
        .transition(id, &RunStatus::Queued, RunStatus::Provisioning)
        .unwrap();
    store
        .transition(id, &RunStatus::Provisioning, RunStatus::Running)
        .unwrap();
}
