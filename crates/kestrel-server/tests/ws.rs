//! Issue #37: the event sockets, with a real server on an ephemeral port and a real client.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::{MemUi, TOKEN, agent, store};
use futures::{SinkExt, StreamExt};
use kestrel_server::{Config, router, serve};
use kestrel_store::Store;
use kestrel_types::{EventKind, RunId, WS_PROTOCOL, WS_TOKEN_PREFIX, WsFrame};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Server {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
    sockets: tokio_util::task::TaskTracker,
}

async fn start(store: Arc<Store>, tune: impl FnOnce(&mut Config)) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = Config::new(TOKEN, addr);
    config.poll = Duration::from_millis(10);
    tune(&mut config);
    let shutdown = CancellationToken::new();
    let sockets = config.sockets.clone();
    let app = router(store, config, Arc::new(MemUi::build()), shutdown.clone());
    let task = tokio::spawn(serve(listener, app, shutdown.clone(), sockets.clone()));
    Server {
        addr,
        shutdown,
        task,
        sockets,
    }
}

/// Opens `path` with the token as a subprotocol.
async fn open(server: &Server, path: &str) -> Socket {
    let mut request = format!("ws://{}{path}", server.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(&format!("{WS_PROTOCOL}, {WS_TOKEN_PREFIX}{TOKEN}")).unwrap(),
    );
    let (socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], WS_PROTOCOL);
    socket
}

/// The next frame, skipping heartbeats unless `heartbeats`.
async fn next(socket: &mut Socket, heartbeats: bool) -> Option<WsFrame> {
    loop {
        let msg = timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a frame within 10 s")?;
        match msg.ok()? {
            Message::Text(text) => {
                let frame: WsFrame = serde_json::from_str(&text).unwrap();
                if heartbeats || frame != WsFrame::Heartbeat {
                    return Some(frame);
                }
            }
            Message::Close(_) => return None,
            _ => {}
        }
    }
}

fn log(store: &Store, run: RunId, line: &str) {
    store
        .append(run, EventKind::AgentLog { line: line.into() })
        .unwrap();
}

#[tokio::test]
async fn a_socket_without_the_token_is_refused() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let server = start(store, |_| {}).await;
    let err = connect_async(format!("ws://{}/ws/runs/{id}", server.addr))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
    let mut request = format!("ws://{}/ws/runs/{id}", server.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("kestrel.v1, kestrel.token.wrong"),
    );
    let err = connect_async(request).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
}

#[tokio::test]
async fn the_token_may_also_come_as_a_query_parameter() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    log(&store, id, "hello");
    let server = start(store, |_| {}).await;
    let (mut socket, _) = connect_async(format!("ws://{}/ws/runs/{id}?token={TOKEN}", server.addr))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut socket, false).await,
        Some(WsFrame::Event { cursor: 1, .. })
    ));
}

#[tokio::test]
async fn a_socket_for_an_unknown_run_is_404() {
    let server = start(store(), |_| {}).await;
    let mut request = format!("ws://{}/ws/runs/{}", server.addr, RunId::new())
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(&format!("{WS_PROTOCOL}, {WS_TOKEN_PREFIX}{TOKEN}")).unwrap(),
    );
    let err = connect_async(request).await.unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");
}

// 7. A socket opened with `after=4` while events are appended gets 5, 6, ... and then the live
//    ones, in order, with no gap and no duplicate, including events that land during the replay.
#[tokio::test]
async fn s7_replay_then_live_with_no_gap_and_no_duplicate() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    for i in 1..=10 {
        log(&store, id, &format!("{i}"));
    }
    let writer = {
        let store = store.clone();
        tokio::spawn(async move {
            for i in 11..=40 {
                log(&store, id, &format!("{i}"));
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
        })
    };
    let server = start(store, |_| {}).await;
    let mut socket = open(&server, &format!("/ws/runs/{id}?after=4")).await;
    let mut seqs = Vec::new();
    while seqs.last() != Some(&40) {
        match next(&mut socket, false).await {
            Some(WsFrame::Event { cursor, event }) => {
                assert_eq!(cursor, event.seq);
                assert_eq!(event.run_id, id);
                seqs.push(event.seq);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    writer.await.unwrap();
    assert_eq!(seqs, (5..=40).collect::<Vec<_>>());
}

// 8. A tree socket gets the events of the three runs in the tree, not the one outside it, in
//    commit order.
#[tokio::test]
async fn s8_a_tree_socket_gets_only_its_tree_in_commit_order() {
    let store = store();
    let root = store.create_run(&agent("lead"), "x").unwrap();
    let one = store.create_child_run(root, &agent("w"), "1").unwrap();
    let two = store.create_child_run(root, &agent("w"), "2").unwrap();
    let outside = store.create_run(&agent("other"), "y").unwrap();
    for round in 0..3 {
        for run in [root, outside, one, two] {
            log(&store, run, &format!("{round}"));
        }
    }
    let server = start(store.clone(), |_| {}).await;
    // Opened through a child: the socket follows the child's whole tree.
    let mut socket = open(&server, &format!("/ws/tree/{two}?after=0")).await;
    let mut got = Vec::new();
    while got.len() < 9 {
        match next(&mut socket, false).await {
            Some(WsFrame::Event { cursor, event }) => got.push((cursor, event.run_id)),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(got.iter().all(|(_, run)| [root, one, two].contains(run)));
    assert!(got.windows(2).all(|w| w[0].0 < w[1].0), "{got:?}");
    // A live event of the outside run never arrives; one of the tree does.
    log(&store, outside, "late");
    log(&store, one, "late");
    match next(&mut socket, false).await {
        Some(WsFrame::Event { event, .. }) => assert_eq!(event.run_id, one),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn heartbeats_keep_a_quiet_socket_alive() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let server = start(store, |c| c.heartbeat = Duration::from_millis(50)).await;
    let mut socket = open(&server, &format!("/ws/runs/{id}")).await;
    assert_eq!(next(&mut socket, true).await, Some(WsFrame::Heartbeat));
}

// 9. A slow client that does not read is dropped with a close frame instead of growing memory.
#[tokio::test]
async fn s9_a_client_that_does_not_read_is_dropped() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let server = start(store.clone(), |c| {
        c.ws_queue = 2;
        c.ws_stall = Duration::from_millis(200);
    })
    .await;
    let mut socket = open(&server, &format!("/ws/runs/{id}")).await;
    let big = "x".repeat(64 * 1024);
    for _ in 0..300 {
        log(&store, id, &big);
    }
    // Not reading for a while lets the server's buffers and the queue fill up.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut events = 0;
    let mut closed = false;
    loop {
        match timeout(Duration::from_secs(10), socket.next()).await {
            Ok(Some(Ok(Message::Text(_)))) => events += 1,
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                closed = true;
                break;
            }
            Ok(Some(Ok(_))) => {}
            Err(_) => break,
        }
    }
    assert!(closed, "the server kept the slow client");
    assert!(events < 300, "got all {events} events");
}

// 16. When the server stops, open sockets get a close frame and the port is released.
#[tokio::test]
async fn s16_stopping_closes_sockets_and_frees_the_port() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let server = start(store, |_| {}).await;
    let mut socket = open(&server, &format!("/ws/runs/{id}")).await;
    // The open socket is tracked, so `serve` waits for it to say goodbye.
    timeout(Duration::from_secs(10), async {
        while server.sockets.len() != 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    server.shutdown.cancel();
    timeout(Duration::from_secs(10), server.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // `serve` returned only after the socket task ended: its close frame is already sent.
    assert!(server.sockets.is_empty());
    let close = timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(frame))) => return frame,
                Some(Ok(_)) => {}
                other => panic!("expected a close frame, got {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(close.is_some());
    TcpListener::bind(server.addr).await.unwrap();
}

// A replay longer than one page is read page after page without waiting a poll tick in between.
#[tokio::test]
async fn a_long_replay_does_not_wait_a_poll_per_page() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    for i in 0..700 {
        log(&store, id, &format!("{i}"));
    }
    let server = start(store, |c| c.poll = Duration::from_secs(30)).await;
    let mut socket = open(&server, &format!("/ws/runs/{id}?after=3")).await;
    let all = timeout(Duration::from_secs(5), async {
        let mut seqs = Vec::new();
        while seqs.len() < 697 {
            match next(&mut socket, false).await {
                Some(WsFrame::Event { event, .. }) => seqs.push(event.seq),
                other => panic!("unexpected {other:?}"),
            }
        }
        seqs
    })
    .await
    .expect("the whole replay well inside one poll interval");
    assert_eq!(all, (4..=700).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_client_close_ends_the_socket_on_the_server() {
    let store = store();
    let id = store.create_run(&agent("a"), "x").unwrap();
    let server = start(store, |_| {}).await;
    let mut socket = open(&server, &format!("/ws/runs/{id}")).await;
    socket.send(Message::Close(None)).await.unwrap();
    // The server answers the close: a clean close handshake, not a dropped connection.
    let answer = timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_))) => return true,
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .unwrap();
    assert!(answer, "the server did not answer the close");
    server.shutdown.cancel();
    timeout(Duration::from_secs(10), server.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

// `serve` waits for tracked sockets after it stops accepting, so each sends its close frame.
#[tokio::test]
async fn serve_waits_for_tracked_sockets_after_it_stops() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config::new(TOKEN, listener.local_addr().unwrap());
    let sockets = config.sockets.clone();
    let shutdown = CancellationToken::new();
    let app = router(store(), config, Arc::new(MemUi::build()), shutdown.clone());
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    sockets.spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        flag.store(true, Ordering::SeqCst);
    });
    shutdown.cancel();
    let started = std::time::Instant::now();
    serve(listener, app, shutdown, sockets).await.unwrap();
    assert!(
        done.load(Ordering::SeqCst),
        "serve returned before the socket ended"
    );
    // It returned as soon as the socket ended, not after the whole grace.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

// ...but not forever: a socket that never ends is given a few seconds.
#[tokio::test(start_paused = true)]
async fn serve_gives_a_stuck_socket_a_bounded_grace() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config::new(TOKEN, listener.local_addr().unwrap());
    let sockets = config.sockets.clone();
    let shutdown = CancellationToken::new();
    let app = router(store(), config, Arc::new(MemUi::build()), shutdown.clone());
    sockets.spawn(std::future::pending::<()>());
    shutdown.cancel();
    let started = tokio::time::Instant::now();
    serve(listener, app, shutdown, sockets).await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(3) && waited < Duration::from_secs(10),
        "{waited:?}"
    );
}

// Issue #41: `/ws/events` follows every run on the global cursor, optionally only some kinds. It
// feeds the approvals inbox and the nav badge on every view.

fn ask(store: &Store, run: RunId) -> kestrel_types::ApprovalId {
    let id = kestrel_types::ApprovalId::new();
    store
        .append(
            run,
            EventKind::ApprovalRequested {
                id,
                action: "git push".into(),
                question: false,
            },
        )
        .unwrap();
    id
}

#[tokio::test]
async fn the_events_socket_follows_every_run_and_only_the_kinds_asked_for() {
    let store = store();
    let a = store.create_run(&agent("a"), "x").unwrap();
    let b = store.create_run(&agent("b"), "y").unwrap();
    log(&store, a, "noise");
    let first = ask(&store, a);
    log(&store, b, "noise");
    let second = ask(&store, b);
    let server = start(store.clone(), |_| {}).await;
    let mut socket = open(&server, "/ws/events?after=0&kinds=approval_requested").await;
    let mut got = Vec::new();
    while got.len() < 2 {
        match next(&mut socket, false).await {
            Some(WsFrame::Event { cursor, event }) => match event.kind {
                EventKind::ApprovalRequested { id, .. } => got.push((cursor, id, event.run_id)),
                other => panic!("unexpected kind {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        got.iter().map(|g| (g.1, g.2)).collect::<Vec<_>>(),
        [(first, a), (second, b)]
    );
    assert!(got[0].0 < got[1].0);
    // Live: a log is skipped, a new request from either tree arrives.
    log(&store, a, "more noise");
    let third = ask(&store, a);
    match next(&mut socket, false).await {
        Some(WsFrame::Event { event, .. }) => {
            assert!(matches!(event.kind, EventKind::ApprovalRequested { id, .. } if id == third));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn without_after_the_events_socket_starts_at_the_newest_event() {
    let store = store();
    let a = store.create_run(&agent("a"), "x").unwrap();
    for i in 0..5 {
        log(&store, a, &format!("old {i}"));
    }
    let server = start(store.clone(), |_| {}).await;
    let mut socket = open(&server, "/ws/events").await;
    // Give the socket a poll or two, so an old event would have been sent by now.
    tokio::time::sleep(Duration::from_millis(50)).await;
    log(&store, a, "new");
    match next(&mut socket, false).await {
        Some(WsFrame::Event { event, .. }) => {
            assert!(matches!(&event.kind, EventKind::AgentLog { line } if line == "new"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn the_events_socket_resumes_after_its_cursor() {
    let store = store();
    let a = store.create_run(&agent("a"), "x").unwrap();
    log(&store, a, "one");
    log(&store, a, "two");
    let server = start(store.clone(), |_| {}).await;
    let mut socket = open(&server, "/ws/events?after=0").await;
    let Some(WsFrame::Event { cursor, .. }) = next(&mut socket, false).await else {
        panic!("no first event");
    };
    drop(socket);
    let mut again = open(&server, &format!("/ws/events?after={cursor}")).await;
    match next(&mut again, false).await {
        Some(WsFrame::Event { event, .. }) => {
            assert!(matches!(&event.kind, EventKind::AgentLog { line } if line == "two"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn the_events_socket_refuses_a_bad_cursor_or_kind() {
    let server = start(store(), |_| {}).await;
    for query in ["after=soon", "kinds=approval_requested,DROP%20TABLE"] {
        let mut request = format!("ws://{}/ws/events?{query}", server.addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(&format!("{WS_PROTOCOL}, {WS_TOKEN_PREFIX}{TOKEN}")).unwrap(),
        );
        let err = connect_async(request).await.unwrap_err();
        assert!(err.to_string().contains("400"), "{query}: {err}");
    }
}
