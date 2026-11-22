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
use tokio_util::task::TaskTracker;

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
    /// The open event sockets. [`serve`] waits for them after it stops accepting, so each one
    /// gets to send its close frame.
    pub sockets: TaskTracker,
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
            sockets: TaskTracker::new(),
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
        // A catch-all never matches an empty tail, so the bare roots (with and without the
        // slash) get their own routes. Otherwise they would fall through to the UI.
        .route("/api", any(api::not_found))
        .route("/api/", any(api::not_found))
        .route("/api/{*rest}", any(api::not_found))
        .route("/ws/runs/{id}", get(ws::run_socket))
        .route("/ws/tree/{id}", get(ws::tree_socket))
        .route("/ws/events", get(ws::events_socket))
        .route("/ws", any(api::not_found))
        .route("/ws/", any(api::not_found))
        .route("/ws/{*rest}", any(api::not_found))
        .route_layer(middleware::from_fn_with_state(state.clone(), guard::auth));
    Router::new()
        .merge(protected)
        .fallback(ui::serve)
        .layer(middleware::from_fn_with_state(state.clone(), guard::origin))
        .with_state(state)
}

/// How long [`serve`] waits for event sockets to send their close frames after it stops.
const SOCKET_GRACE: Duration = Duration::from_secs(3);

/// Serves `app` on `listener` until `shutdown` is cancelled. Then it waits for HTTP requests in
/// flight, and (up to a few seconds) for the event sockets in `sockets` (`Config::sockets` of the
/// router) to send their close frames, and releases the port.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: CancellationToken,
    sockets: TaskTracker,
) -> io::Result<()> {
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await;
    sockets.close();
    let _ = tokio::time::timeout(SOCKET_GRACE, sockets.wait()).await;
    served
}

/// Shortest token accepted from the token file.
const MIN_TOKEN_LEN: usize = 32;

/// The API token stored at `path`, made on first use: 32 random bytes as 64 hex characters, in a
/// file only its owner can read (mode 0600 on Unix). A token found there must be at least 32
/// characters of letters, digits, `-` and `_`, which fit in a WebSocket subprotocol; on Unix
/// the file is set to 0600 if others could read it.
/// Every error names the file.
pub fn load_or_create_token(path: &Path) -> io::Result<String> {
    load_or_create(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

fn load_or_create(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let token = text.trim();
            let usable = token.len() >= MIN_TOKEN_LEN
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !usable {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "the token must be at least {MIN_TOKEN_LEN} letters, digits, - or _; delete the file to make a new one"
                    ),
                ));
            }
            tighten(path)?;
            Ok(token.to_string())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_token(path),
        Err(e) => Err(e),
    }
}

/// Makes the token file readable by its owner only, if others could read it.
#[cfg(unix)]
fn tighten(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten(_path: &Path) -> io::Result<()> {
    Ok(())
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
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => load_or_create(path),
        Err(e) => Err(e),
    }
}
