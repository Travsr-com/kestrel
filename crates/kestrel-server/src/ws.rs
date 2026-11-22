//! Event sockets: `/ws/runs/{id}?after=<seq>` follows one run, `/ws/tree/{id}?after=<cursor>`
//! follows the whole tree of `id`'s root on the global event cursor, and
//! `/ws/events?after=<cursor>&kinds=<kind,...>` follows every run, optionally only some kinds.
//!
//! A socket first replays what is after the cursor, then sends new events as they are committed,
//! each as a [`WsFrame::Event`] with the cursor to resume from. It reads the store from its own
//! cursor every `poll`, so an event committed during the replay is sent next: no gap, no
//! duplicate. A heartbeat goes out every `heartbeat`.
//!
//! Frames go through a queue of `ws_queue` to a writer task. A client that does not read fills
//! it; once it stays full for `ws_stall`, the client is dropped with a close frame, so memory
//! never grows without bound. A client that starts reading again within `ws_close_grace` gets
//! that close frame; one that does not is cut then. When the server stops, every socket sends a
//! close frame and ends, giving a client that does not take it [`CLOSE_GRACE`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use kestrel_store::{Store, StoreError};
use kestrel_types::{RunId, WS_PROTOCOL, WsFrame};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval, interval_at, timeout};
use tokio_util::sync::CancellationToken;

use crate::AppState;
use crate::api::{Reject, after, error, find_run, internal};
use axum::http::StatusCode;

/// What a socket follows.
#[derive(Debug, Clone)]
enum Source {
    /// One run; the cursor is the event `seq`.
    Run(RunId),
    /// A whole tree by its root; the cursor is the global event cursor.
    Tree(RunId),
    /// Every run, only the given kinds (all when empty); the cursor is the global event cursor.
    All(Arc<[String]>),
}

/// How a socket ended, for its close frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The client closed or went away: nothing more to send.
    Client,
    /// The server is stopping.
    Shutdown,
    /// The client did not read for `ws_stall`.
    TooSlow,
    /// The store failed.
    Error,
}

type Params = Query<HashMap<String, String>>;

/// How long a socket tries to send its close frame when the server stops or the store fails.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// `GET /ws/runs/{id}?after=<seq>`.
pub(crate) async fn run_socket(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Params,
    ws: WebSocketUpgrade,
) -> Response {
    let row = match find_run(&s.store, &id) {
        Ok(row) => row,
        Err(res) => return *res,
    };
    match after(&q) {
        Ok(cursor) => upgrade(ws, s, Source::Run(row.id), cursor),
        Err(res) => *res,
    }
}

/// `GET /ws/tree/{id}?after=<cursor>`: the tree of `id`'s root, so any run of a tree opens it.
pub(crate) async fn tree_socket(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Params,
    ws: WebSocketUpgrade,
) -> Response {
    let row = match find_run(&s.store, &id) {
        Ok(row) => row,
        Err(res) => return *res,
    };
    match after(&q) {
        Ok(cursor) => upgrade(ws, s, Source::Tree(row.root), cursor),
        Err(res) => *res,
    }
}

/// `GET /ws/events?after=<cursor>&kinds=<kind,...>`: every run's events on the global cursor. Without
/// `after` it starts at the newest event, so a client that only wants what comes next does not
/// replay the whole history. `kinds` are event kind names such as `approval_requested`.
pub(crate) async fn events_socket(
    State(s): State<AppState>,
    Query(q): Params,
    ws: WebSocketUpgrade,
) -> Response {
    let kinds = match kinds(&q) {
        Ok(kinds) => kinds,
        Err(res) => return *res,
    };
    let cursor = if q.contains_key("after") {
        match after(&q) {
            Ok(cursor) => cursor,
            Err(res) => return *res,
        }
    } else {
        match s.store.latest_cursor() {
            Ok(cursor) => cursor,
            Err(e) => return internal(e),
        }
    };
    upgrade(ws, s, Source::All(kinds), cursor)
}

/// The `kinds` filter: a comma-separated list of event kind names. Empty items are skipped; a name
/// that is not lower-case letters and underscores, or a list that names none, is a 400. A
/// well-formed name of no kind simply matches nothing.
fn kinds(q: &HashMap<String, String>) -> Result<Arc<[String]>, Reject> {
    let Some(list) = q.get("kinds") else {
        return Ok(Arc::from(Vec::new()));
    };
    let bad = || {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "kinds must be event kind names like approval_requested, separated by commas",
        ))
    };
    let mut kinds = Vec::new();
    for kind in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        if !kind.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            return Err(bad());
        }
        kinds.push(kind.to_owned());
    }
    // `kinds=` naming nothing is a mistake, not "every kind".
    if kinds.is_empty() {
        return Err(bad());
    }
    Ok(Arc::from(kinds))
}

fn upgrade(ws: WebSocketUpgrade, s: AppState, source: Source, cursor: u64) -> Response {
    // Echoes `kestrel.v1` when the client offers it; the token subprotocol is never echoed.
    // Tracked, so `serve` lets the socket send its close frame before the server goes.
    let sockets = s.config.sockets.clone();
    ws.protocols([WS_PROTOCOL])
        .on_upgrade(move |socket| sockets.track_future(follow(socket, s, source, cursor)))
}

/// Events read from the store per step, so a socket never holds a whole history at once.
const PAGE: usize = 256;

/// Events after `cursor`, each with its cursor, and where to read from next (past events a
/// filter skipped, so they are not scanned again).
fn read(
    store: &Store,
    source: &Source,
    cursor: u64,
) -> Result<(Vec<(u64, kestrel_types::Event)>, u64), StoreError> {
    let events = match source {
        Source::Run(run) => store
            .events_page(*run, cursor, PAGE)?
            .into_iter()
            .map(|e| (e.seq, e))
            .collect(),
        Source::Tree(root) => store.events_since_global_page(*root, cursor, PAGE)?,
        Source::All(kinds) => {
            let kinds: Vec<&str> = kinds.iter().map(String::as_str).collect();
            let page = store.events_after_page(cursor, &kinds, PAGE)?;
            return Ok((page.events, page.next));
        }
    };
    let next = events.last().map_or(cursor, |(at, _)| *at);
    Ok((events, next))
}

fn text(frame: &WsFrame) -> Option<Message> {
    serde_json::to_string(frame)
        .ok()
        .map(|t| Message::Text(t.into()))
}

async fn follow(socket: WebSocket, s: AppState, source: Source, mut cursor: u64) {
    let (mut sink, mut incoming) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(s.config.ws_queue.max(1));
    let stop = CancellationToken::new();
    let ending = stop.clone();
    let (end_tx, end_rx) = tokio::sync::oneshot::channel::<End>();
    let slow_grace = s.config.ws_close_grace;
    let writer = tokio::spawn(async move {
        let mut end_rx = end_rx;
        loop {
            let next = tokio::select! {
                _ = ending.cancelled() => break,
                m = rx.recv() => m,
            };
            let Some(msg) = next else { break };
            let sent = tokio::select! {
                _ = ending.cancelled() => break,
                r = sink.send(msg) => r,
            };
            if sent.is_err() {
                return;
            }
        }
        let close = match end_rx.try_recv() {
            Ok(End::Shutdown) => Some((close_code::AWAY, "kestrel is stopping")),
            Ok(End::TooSlow) => Some((close_code::POLICY, "the client is not reading")),
            Ok(End::Error) => Some((close_code::ERROR, "store error")),
            // The client closed. The socket queued its answer when it read the client's close
            // frame; closing the sink below flushes it, so the client sees a clean handshake.
            Ok(End::Client) | Err(_) => None,
        };
        // A client that does not read may never take it: give up after a while. A slow client
        // gets `ws_close_grace` to read again, so it learns why it was dropped instead of seeing
        // the connection reset under it.
        let grace = match close {
            Some((code, _)) if code == close_code::POLICY => slow_grace,
            _ => CLOSE_GRACE,
        };
        let _ = timeout(grace, async {
            if let Some((code, reason)) = close {
                let frame = CloseFrame {
                    code,
                    reason: reason.into(),
                };
                sink.send(Message::Close(Some(frame))).await?;
            }
            sink.close().await
        })
        .await;
    });

    let end = pump(&s, &source, &mut cursor, &tx, &mut incoming).await;
    let _ = end_tx.send(end);
    stop.cancel();
    let _ = writer.await;
}

/// Feeds the queue until the socket should end, and says why.
async fn pump(
    s: &AppState,
    source: &Source,
    cursor: &mut u64,
    tx: &mpsc::Sender<Message>,
    incoming: &mut futures::stream::SplitStream<WebSocket>,
) -> End {
    let beat = s.config.heartbeat;
    let mut heartbeat = interval_at(tokio::time::Instant::now() + beat, beat);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut poll = interval(s.config.poll);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = s.shutdown.cancelled() => return End::Shutdown,
            msg = incoming.next() => match msg {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return End::Client,
                // Pings are answered by the socket itself; anything else from the client is ignored.
                Some(Ok(_)) => {}
            },
            _ = heartbeat.tick() => {
                if let Some(end) = push(s, tx, &WsFrame::Heartbeat).await {
                    return end;
                }
            }
            _ = poll.tick() => {
                let (events, next) = match read(&s.store, source, *cursor) {
                    Ok(read) => read,
                    Err(_) => return End::Error,
                };
                let full = events.len() == PAGE;
                for (at, event) in events {
                    if let Some(end) = push(s, tx, &WsFrame::Event { cursor: at, event }).await {
                        return end;
                    }
                    *cursor = at;
                }
                *cursor = (*cursor).max(next);
                // A full page means more is waiting: read the next one without waiting a tick.
                if full {
                    poll.reset_immediately();
                }
            }
        }
    }
}

/// Queues one frame. `Some(end)` when the socket should end instead: the server is stopping, the
/// writer is gone, or the queue stayed full for `ws_stall`.
async fn push(s: &AppState, tx: &mpsc::Sender<Message>, frame: &WsFrame) -> Option<End> {
    let msg = text(frame)?;
    tokio::select! {
        _ = s.shutdown.cancelled() => Some(End::Shutdown),
        sent = timeout(s.config.ws_stall, tx.send(msg)) => match sent {
            Ok(Ok(())) => None,
            Ok(Err(_)) => Some(End::Client),
            Err(_) => Some(End::TooSlow),
        },
    }
}
