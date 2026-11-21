//! Event sockets: `/ws/runs/{id}?after=<seq>` follows one run, `/ws/tree/{id}?after=<cursor>`
//! follows the whole tree of `id`'s root on the global event cursor.
//!
//! A socket first replays what is after the cursor, then sends new events as they are committed,
//! each as a [`WsFrame::Event`] with the cursor to resume from. It reads the store from its own
//! cursor every `poll`, so an event committed during the replay is sent next: no gap, no
//! duplicate. A heartbeat goes out every `heartbeat`.
//!
//! Frames go through a queue of `ws_queue` to a writer task. A client that does not read fills
//! it; once it stays full for `ws_stall`, the client is dropped with a close frame, so memory
//! never grows without bound. When the server stops, every socket sends a close frame and ends.

use std::collections::HashMap;
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
use crate::api::{after, find_run};

/// What a socket follows.
#[derive(Debug, Clone, Copy)]
enum Source {
    /// One run; the cursor is the event `seq`.
    Run(RunId),
    /// A whole tree by its root; the cursor is the global event cursor.
    Tree(RunId),
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

fn upgrade(ws: WebSocketUpgrade, s: AppState, source: Source, cursor: u64) -> Response {
    // Echoes `kestrel.v1` when the client offers it; the token subprotocol is never echoed.
    // Tracked, so `serve` lets the socket send its close frame before the server goes.
    let sockets = s.config.sockets.clone();
    ws.protocols([WS_PROTOCOL])
        .on_upgrade(move |socket| sockets.track_future(follow(socket, s, source, cursor)))
}

/// Events read from the store per step, so a socket never holds a whole history at once.
const PAGE: usize = 256;

/// Events after `cursor`, each with its cursor.
fn read(
    store: &Store,
    source: Source,
    cursor: u64,
) -> Result<Vec<(u64, kestrel_types::Event)>, StoreError> {
    match source {
        Source::Run(run) => Ok(store
            .events_page(run, cursor, PAGE)?
            .into_iter()
            .map(|e| (e.seq, e))
            .collect()),
        Source::Tree(root) => store.events_since_global_page(root, cursor, PAGE),
    }
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
        // A client that does not read may never take it: give up after a moment.
        let _ = timeout(Duration::from_secs(1), async {
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

    let end = pump(&s, source, &mut cursor, &tx, &mut incoming).await;
    let _ = end_tx.send(end);
    stop.cancel();
    let _ = writer.await;
}

/// Feeds the queue until the socket should end, and says why.
async fn pump(
    s: &AppState,
    source: Source,
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
                let events = match read(&s.store, source, *cursor) {
                    Ok(events) => events,
                    Err(_) => return End::Error,
                };
                let full = events.len() == PAGE;
                for (at, event) in events {
                    if let Some(end) = push(s, tx, &WsFrame::Event { cursor: at, event }).await {
                        return end;
                    }
                    *cursor = at;
                }
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
