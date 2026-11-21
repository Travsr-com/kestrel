//! HTTP and WebSocket API for the UI and the CLI (`docs/api.md`), and the embedded UI.
//!
//! A separate listener from the gateway, so a sandbox cannot reach it. Every `/api` and `/ws`
//! route needs the bearer token. Every request must name the bound address in `Host` (and, when
//! a browser sends one, `Origin`), so a web page on another site, or a DNS name rebound to
//! 127.0.0.1, cannot call it. There is no CORS. Commands go through the store's control channel,
//! like the CLI's, so the engine treats both the same.

mod api;
mod guard;
mod ui;
mod ws;

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::middleware;
use axum::routing::{any, get, post};
use kestrel_store::Store;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use ui::{Asset, Assets, EmbeddedUi, PLACEHOLDER};

/// Settings of one API server.
#[derive(Debug, Clone)]
pub struct Config {
    /// The bearer token every `/api` and `/ws` request must carry.
    pub token: String,
    /// The address the server listens on. `Host` and `Origin` must name it.
    pub bind: SocketAddr,
    /// How often an event socket sends a heartbeat. 15 s by default.
    pub heartbeat: Duration,
    /// How often an event socket checks the store for new events. 200 ms by default.
    pub poll: Duration,
    /// Frames an event socket may hold for a client that is not reading. 256 by default.
    pub ws_queue: usize,
    /// How long a full queue may stay full before the client is dropped. 10 s by default.
    pub ws_stall: Duration,
}

impl Config {
    pub fn new(token: impl Into<String>, bind: SocketAddr) -> Config {
        Config {
            token: token.into(),
            bind,
            heartbeat: Duration::from_secs(15),
            poll: Duration::from_millis(200),
            ws_queue: 256,
            ws_stall: Duration::from_secs(10),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) store: Arc<Store>,
    pub(crate) config: Arc<Config>,
    pub(crate) ui: Arc<dyn Assets>,
    /// Cancelled when the server stops: open event sockets send a close frame and end.
    pub(crate) shutdown: CancellationToken,
}

/// The whole API and the UI. `shutdown` is the same token passed to [`serve`].
pub fn router(
    store: Arc<Store>,
    config: Config,
    ui: Arc<dyn Assets>,
    shutdown: CancellationToken,
) -> Router {
    let state = AppState {
        store,
        config: Arc::new(config),
        ui,
        shutdown,
    };
    let protected = Router::new()
        .route("/api/runs", get(api::list_runs))
        .route("/api/runs/{id}", get(api::get_run))
        .route("/api/runs/{id}/tree", get(api::tree))
        .route("/api/runs/{id}/events", get(api::events))
        .route("/api/runs/{id}/{action}", post(api::command))
        .route("/api/approvals", get(api::approvals))
        .route("/api/approvals/{id}/{action}", post(api::resolve))
        .route("/api", any(api::not_found))
        .route("/api/{*rest}", any(api::not_found))
        .route("/ws/runs/{id}", get(ws::run_socket))
        .route("/ws/tree/{id}", get(ws::tree_socket))
        .route_layer(middleware::from_fn_with_state(state.clone(), guard::auth));
    Router::new()
        .merge(protected)
        .fallback(ui::serve)
        .layer(middleware::from_fn_with_state(state.clone(), guard::origin))
        .with_state(state)
}

/// Serves `app` on `listener` until `shutdown` is cancelled, then waits for open connections
/// (event sockets close themselves on the same token) and releases the port.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: CancellationToken,
) -> io::Result<()> {
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
}

/// The API token stored at `path`, made on first use: 32 random bytes as 64 hex characters, in a
/// file only its owner can read (mode 0600 on Unix). Hex fits in a WebSocket subprotocol.
pub fn load_or_create_token(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let token = text.trim();
            if token.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is empty; delete it to make a new token", path.display()),
                ));
            }
            Ok(token.to_string())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_token(path),
        Err(e) => Err(e),
    }
}

fn create_token(path: &Path) -> io::Result<String> {
    use std::io::Write;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            Ok(token)
        }
        // Another process made it first: use theirs.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => load_or_create_token(path),
        Err(e) => Err(e),
    }
}
