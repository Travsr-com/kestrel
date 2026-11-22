//! Issue #25, scenarios 1 to 4: `kestrel serve` stops within `engine.shutdown_grace_secs`
//! (default 10, integer 1 to 3600), whatever is in flight, on SIGINT or SIGTERM.
//!
//! Pinned behaviour:
//! * First SIGINT or SIGTERM: `Engine::shutdown(grace)`, then the gateway is stopped with the
//!   same overall bound, then the bridge, then exit. One paragraph on stderr starting with
//!   `shutdown:` says how many sandboxes were destroyed and lists failed or timed-out runs.
//!   Exit 0 when all was cleaned up, 1 when the report has failed or timed-out runs.
//! * A second signal (INT or TERM) while shutting down exits at once with code 130, without
//!   waiting for anything, and does not try to remove the bridge.
//! * A signal while driver detection hangs still exits within the grace period, with code 0
//!   (nothing was started, so nothing is left behind).
//!
//! No real Docker: the fake driver, a stub `docker` script, and a fake upstream model API that
//! sends one streamed chunk and then goes silent, so a gateway request stays in flight.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, ExitStatus};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{ChildGuard, Fixture, path_str, poll};
use kestrel_types::{RunId, RunStatus, SuspendReason};
use predicates::prelude::*;

const GRACE: u64 = 2;
/// Slack on top of the grace period for process start-up and the exit itself.
const SLACK: Duration = Duration::from_secs(3);

fn signal(serve: &ChildGuard, name: &str) {
    let status = Command::new("kill")
        .args(["-s", name, &serve.child.id().to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -s {name} failed");
}

/// Waits up to `limit` for the child to exit.
fn exits_within(serve: &mut ChildGuard, limit: Duration) -> Option<ExitStatus> {
    let end = Instant::now() + limit;
    loop {
        if let Some(s) = serve.child.try_wait().expect("try_wait") {
            return Some(s);
        }
        if Instant::now() >= end {
            return None;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// A fake upstream: accepts one connection, sends response headers and one chunk, then holds
/// the connection open without sending more. `accepted` fires once the request arrived.
fn stalled_upstream() -> (u16, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Ok((mut conn, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 4096];
        let _ = conn.read(&mut buf);
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                    transfer-encoding: chunked\r\n\r\nd\r\ndata: hello\n\n\r\n";
        let _ = conn.write_all(head.as_bytes());
        let _ = tx.send(());
        // Hold the stream open, silently, until the test process ends.
        thread::sleep(Duration::from_secs(120));
    });
    (port, rx)
}

fn config(upstream: Option<u16>, extra_engine: &str) -> String {
    let base = upstream.map_or(String::new(), |p| {
        format!("base_url = \"http://127.0.0.1:{p}\"\n")
    });
    format!(
        "[gateway]\nlisten = \"127.0.0.1:0\"\n\n[engine]\n{extra_engine}\n\
         [providers.anthropic]\n{base}api_key = \"test-key\"\n"
    )
}

struct Serving {
    serve: ChildGuard,
    gateway: String,
}

fn start(f: &Fixture, config: &str, driver_args: &[&str], path: Option<&str>) -> Serving {
    let cfg = f.write("config.toml", config);
    let mut cmd = f.kestrel_std();
    cmd.args(["serve", "--config", path_str(&cfg)])
        .args(driver_args);
    if let Some(p) = path {
        cmd.env("PATH", p);
    }
    let mut serve = ChildGuard::spawn(cmd);
    let line = serve
        .stdout_lines()
        .until("serve to be running", |l| l.contains("kestrel is running"));
    let addr = line
        .split("http://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .expect("gateway address in the ready line")
        .to_string();
    Serving {
        serve,
        gateway: addr,
    }
}

fn start_fake(f: &Fixture, config: &str) -> Serving {
    start(f, config, &["--sandbox-driver", "fake"], None)
}

/// A live run whose token the test's model request uses. It is made before serve starts and
/// walked to a manual pause: serve never claims or moves a suspended run, so nothing races serve
/// (issue #81), and a suspended run keeps its token (issue #88: only an ended run's token stops
/// working on the gateway).
fn live_run(f: &Fixture) -> RunId {
    let store = f.store();
    let run = store
        .create_run(&common::agent(), "task")
        .expect("create run");
    let mut from = RunStatus::Queued;
    for next in [
        RunStatus::Provisioning,
        RunStatus::Running,
        RunStatus::Suspended(SuspendReason::Manual),
    ] {
        store
            .transition(run, &from, next.clone())
            .expect("walk the run before serve starts");
        from = next;
    }
    run
}

/// A new `Queued` run, made while serve runs: serve claims it and fails it (the fixture agent's
/// repo is not an absolute path).
fn queued_run(f: &Fixture) -> RunId {
    f.store()
        .create_run(&common::agent(), "task")
        .expect("create run")
}

/// Sends a model request through the gateway for `run` and returns the open connection.
fn send_model_request(f: &Fixture, s: &Serving, run: RunId) -> TcpStream {
    let token = f.store().run_token(run).expect("token").expect("has token");
    let body = r#"{"model":"claude-sonnet-5-5","stream":true,"max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
    let mut conn = TcpStream::connect(&s.gateway).expect("connect to gateway");
    let req = format!(
        "POST /r/{token}/v1/messages HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n{body}",
        body.len()
    );
    conn.write_all(req.as_bytes()).expect("send request");
    conn
}

/// Starts a streaming model request through the gateway for `run` and returns the open
/// connection once the upstream has the request, so the request is certainly in flight.
fn request_in_flight(
    f: &Fixture,
    s: &Serving,
    accepted: &mpsc::Receiver<()>,
    run: RunId,
) -> TcpStream {
    let conn = send_model_request(f, s, run);
    accepted
        .recv_timeout(Duration::from_secs(10))
        .expect("the request reached the upstream and is in flight");
    conn
}

fn assert_summary(stderr: &str) {
    assert!(
        stderr.contains("shutdown:"),
        "no shutdown summary: {stderr}"
    );
    assert!(stderr.contains("destroyed"), "no destroyed count: {stderr}");
}

// Scenario 1

#[test]
fn sigint_with_a_streaming_request_in_flight_exits_within_the_grace_period() {
    let f = Fixture::new();
    let (port, accepted) = stalled_upstream();
    let run = live_run(&f);
    let mut s = start_fake(
        &f,
        &config(Some(port), &format!("shutdown_grace_secs = {GRACE}")),
    );
    let _conn = request_in_flight(&f, &s, &accepted, run);

    let t = Instant::now();
    signal(&s.serve, "INT");
    let status = exits_within(&mut s.serve, Duration::from_secs(GRACE) + SLACK)
        .unwrap_or_else(|| panic!("serve did not exit within the grace period (+slack)"));
    assert!(t.elapsed() < Duration::from_secs(GRACE) + SLACK);
    assert_eq!(status.code(), Some(0), "everything was cleaned up");
    assert_summary(&s.serve.stderr());
}

#[test]
fn an_idle_serve_stops_at_once_with_exit_0_and_a_summary() {
    let f = Fixture::new();
    let mut s = start_fake(&f, &config(None, ""));
    let t = Instant::now();
    signal(&s.serve, "INT");
    let status = exits_within(&mut s.serve, Duration::from_secs(5)).expect("exit");
    assert_eq!(status.code(), Some(0));
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_summary(&s.serve.stderr());
}

#[test]
fn the_default_grace_is_ten_seconds_so_a_held_request_does_not_outlive_it() {
    let f = Fixture::new();
    let (port, accepted) = stalled_upstream();
    let run = live_run(&f);
    let mut s = start_fake(&f, &config(Some(port), ""));
    let _conn = request_in_flight(&f, &s, &accepted, run);
    signal(&s.serve, "INT");
    // Not yet exited after 5s only if serve waits for the grace; must be gone by 10s + slack.
    let status = exits_within(&mut s.serve, Duration::from_secs(10) + SLACK).expect("exit");
    assert_eq!(status.code(), Some(0));
}

/// Issue #88: once serve has failed a run (the fixture agent's repo is not an absolute path),
/// its token no longer works on the gateway: a model request with it is refused with 410 at
/// once, nothing is held open, and a shutdown right after does not wait for anything.
#[test]
fn a_run_that_has_ended_cannot_hold_a_request_open() {
    let f = Fixture::new();
    let (port, accepted) = stalled_upstream();
    let mut s = start_fake(
        &f,
        // A long grace, so stopping well within it shows nothing was waited for.
        &config(Some(port), "shutdown_grace_secs = 10"),
    );
    let run = queued_run(&f);
    poll("serve to claim and fail the run", || {
        let row = f
            .store()
            .get_run(run)
            .expect("read run")
            .expect("run exists");
        matches!(row.status, RunStatus::Failed(_)).then_some(())
    });
    let mut conn = send_model_request(&f, &s, run);
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut head = [0u8; 12];
    conn.read_exact(&mut head).expect("an answer at once");
    assert_eq!(
        &head, b"HTTP/1.1 410",
        "an ended run's model call is refused"
    );
    assert!(
        accepted.recv_timeout(Duration::from_millis(300)).is_err(),
        "nothing reached the upstream"
    );

    let t = Instant::now();
    signal(&s.serve, "INT");
    let status = exits_within(&mut s.serve, Duration::from_secs(10) + SLACK)
        .unwrap_or_else(|| panic!("serve did not exit within the grace period (+slack)"));
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "nothing was in flight, so serve did not wait for the grace"
    );
    assert_eq!(status.code(), Some(0), "everything was cleaned up");
    assert_summary(&s.serve.stderr());
}

#[test]
fn a_bad_shutdown_grace_exits_2_naming_the_key_and_never_echoing_the_value() {
    for bad in ["0", "3601", "987654321", "\"sekrit-grace\"", "1.5"] {
        let f = Fixture::new();
        let cfg = f.write(
            "config.toml",
            &config(None, &format!("shutdown_grace_secs = {bad}")),
        );
        f.kestrel()
            .args([
                "serve",
                "--config",
                path_str(&cfg),
                "--sandbox-driver",
                "fake",
            ])
            .timeout(common::DEADLINE)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("engine.shutdown_grace_secs"))
            .stderr(predicate::str::contains("987654321").not())
            .stderr(predicate::str::contains("sekrit-grace").not());
    }
}

// Scenario 2

fn second_signal_aborts(first: &str, second: &str) {
    let f = Fixture::new();
    let (port, accepted) = stalled_upstream();
    let run = live_run(&f);
    // A long grace, so the first signal's shutdown is still waiting on the held request.
    let mut s = start_fake(&f, &config(Some(port), "shutdown_grace_secs = 60"));
    let _conn = request_in_flight(&f, &s, &accepted, run);

    signal(&s.serve, first);
    thread::sleep(Duration::from_millis(700));
    assert!(
        s.serve.child.try_wait().expect("try_wait").is_none(),
        "the first signal waits for the in-flight request, it does not exit"
    );
    let t = Instant::now();
    signal(&s.serve, second);
    let status = exits_within(&mut s.serve, Duration::from_secs(3))
        .expect("the second signal exits at once");
    assert_eq!(status.code(), Some(130), "{first} then {second}");
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn a_second_sigint_exits_130_without_waiting() {
    second_signal_aborts("INT", "INT");
}

#[test]
fn sigint_then_sigterm_exits_130_without_waiting() {
    second_signal_aborts("INT", "TERM");
}

#[test]
fn sigterm_then_sigint_exits_130_without_waiting() {
    second_signal_aborts("TERM", "INT");
}

// Scenario 3

#[test]
fn sigterm_behaves_like_the_first_sigint_with_a_request_in_flight() {
    let f = Fixture::new();
    let (port, accepted) = stalled_upstream();
    let run = live_run(&f);
    let mut s = start_fake(
        &f,
        &config(Some(port), &format!("shutdown_grace_secs = {GRACE}")),
    );
    let _conn = request_in_flight(&f, &s, &accepted, run);
    signal(&s.serve, "TERM");
    let status = exits_within(&mut s.serve, Duration::from_secs(GRACE) + SLACK)
        .expect("exit within the grace period");
    assert_eq!(status.code(), Some(0));
    assert_summary(&s.serve.stderr());
}

#[test]
fn sigterm_on_an_idle_serve_is_a_clean_exit_0_with_a_summary() {
    let f = Fixture::new();
    let mut s = start_fake(&f, &config(None, ""));
    signal(&s.serve, "TERM");
    let status = exits_within(&mut s.serve, Duration::from_secs(5)).expect("exit");
    assert_eq!(status.code(), Some(0), "not killed by the signal");
    assert_summary(&s.serve.stderr());
}

// Scenario 4

#[cfg(unix)]
#[test]
fn sigint_while_docker_detection_hangs_still_exits_within_the_grace_period() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let bin = f.dir.path().join("bin");
    std::fs::create_dir(&bin).expect("bin dir");
    let docker = bin.join("docker");
    std::fs::write(&docker, "#!/bin/sh\nexec sleep 60\n").expect("stub");
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!("{}:/usr/bin:/bin", bin.display());

    let cfg = f.write(
        "config.toml",
        &config(None, &format!("shutdown_grace_secs = {GRACE}")),
    );
    let mut cmd = f.kestrel_std();
    cmd.args(["serve", "--config", path_str(&cfg)])
        .env("PATH", path);
    let mut serve = ChildGuard::spawn(cmd);
    // Detection is stuck on `docker info`: serve never reaches "running".
    thread::sleep(Duration::from_millis(1500));
    assert!(
        serve.child.try_wait().expect("try_wait").is_none(),
        "serve should still be waiting on the stub docker"
    );
    let t = Instant::now();
    signal(&serve, "INT");
    let status = exits_within(&mut serve, Duration::from_secs(GRACE) + SLACK)
        .expect("serve exits within the grace period although docker hangs");
    assert!(t.elapsed() < Duration::from_secs(GRACE) + SLACK);
    assert_eq!(
        status.code(),
        Some(0),
        "nothing was started, nothing is left"
    );
}
