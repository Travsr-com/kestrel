//! Issue #37: `kestrel up` runs the engine, the gateway and the API server, prints the UI address
//! with the token, keeps the token in a file next to the database, and stops on SIGINT.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;

use common::{ChildGuard, Fixture, path_str};

const CONFIG: &str = r#"
[gateway]
listen = "127.0.0.1:0"

[providers.anthropic]
base_url = "http://127.0.0.1:9"
api_key = "test-anthropic-key"
"#;

/// A plain HTTP/1.1 GET; returns the status code and body.
fn http_get(addr: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn up_serves_the_api_with_the_printed_token_and_stops_on_sigint() {
    let f = Fixture::new();
    let config = f.write("config.toml", CONFIG);
    let mut cmd = f.kestrel_std();
    cmd.args([
        "up",
        "--config",
        path_str(&config),
        "--sandbox-driver",
        "fake",
        "--listen",
        "127.0.0.1:0",
        "--no-open",
        "--json",
    ]);
    let mut up = ChildGuard::spawn(cmd);
    let mut lines = up.stdout_lines();
    let ready = common::json_line(&lines.next_line("up ready line"));
    let api = ready["api"].as_str().expect("api address").to_string();
    let url = ready["url"].as_str().expect("ui url").to_string();
    assert!(
        api.starts_with("127.0.0.1:") && !api.ends_with(":0"),
        "{ready}"
    );
    assert!(ready["gateway"].is_string(), "{ready}");

    let token = url
        .split("?token=")
        .nth(1)
        .expect("token in the url")
        .to_string();
    assert_eq!(url, format!("http://{api}/?token={token}"));
    let stored = std::fs::read_to_string(f.db.with_file_name("server.token")).unwrap();
    assert_eq!(stored.trim(), token);

    let (status, body) = http_get(&api, "/api/runs", Some(&token));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.trim_end(), "[]");
    assert_eq!(http_get(&api, "/api/runs", None).0, 401);
    // The UI (or its placeholder) is served without the token: it holds no data.
    assert_eq!(http_get(&api, "/", None).0, 200);

    up.interrupt();
    let status = up.wait_exit("up to stop after SIGINT");
    assert!(
        status.success(),
        "up exit {status:?}, stderr: {}",
        up.stderr()
    );
    // The port is free again.
    std::net::TcpListener::bind(&api).unwrap();
}

#[test]
fn up_reuses_the_token_across_starts() {
    let f = Fixture::new();
    std::fs::write(f.db.with_file_name("server.token"), "feedface".repeat(8)).unwrap();
    let config = f.write("config.toml", CONFIG);
    let mut cmd = f.kestrel_std();
    cmd.args([
        "up",
        "--config",
        path_str(&config),
        "--sandbox-driver",
        "fake",
        "--listen",
        "127.0.0.1:0",
        "--no-open",
        "--json",
    ]);
    let mut up = ChildGuard::spawn(cmd);
    let ready = common::json_line(&up.stdout_lines().next_line("up ready line"));
    assert!(
        ready["url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("?token={}", "feedface".repeat(8))),
        "{ready}"
    );
    up.interrupt();
    assert!(up.wait_exit("up to stop").success());
}
