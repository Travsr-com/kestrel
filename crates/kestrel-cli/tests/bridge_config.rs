//! `kestrel serve` and the gateway bridge (issue #8): the `[gateway] advertise_url` setting, the
//! non-loopback warning, and "the fake driver never starts a bridge".
//!
//! How "no bridge" is observed: a bridge is started by running `docker` commands. Each test that
//! serves puts a stub `docker` first on `PATH` that appends its argv to a log file. With the
//! debug-only fake driver the log must never appear. (The stub also makes sure a regression
//! cannot touch the real docker of the machine running the tests.)
//!
//! Config under test:
//!
//! ```toml
//! [gateway]
//! listen = "127.0.0.1:7070"
//! advertise_url = "http://kestrel-gateway:7070/"   # optional, absolute http(s) URL
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::PathBuf;

use common::{ChildGuard, DEADLINE, Fixture, path_str};
use predicates::prelude::*;

const BASE: &str = "[providers.anthropic]\napi_key = \"test-key\"\n";
const SECRET_HOST: &str = "secret-host-9f3a";

fn config(gateway: &str) -> String {
    format!("[gateway]\nlisten = \"127.0.0.1:0\"\n{gateway}\n{BASE}")
}

/// Installs the stub `docker` and returns (its log path, the PATH to use).
fn docker_stub(f: &Fixture) -> (PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let bin = f.dir.path().join("stub-bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    let log = f.dir.path().join("docker-calls.log");
    let script = format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", log.display());
    let exe = bin.join("docker");
    std::fs::write(&exe, script).expect("stub");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (log, path)
}

/// `kestrel serve` with the fake driver, run to completion (for config errors).
fn serve_once(f: &Fixture, config: &str) -> assert_cmd::assert::Assert {
    let path = f.write("config.toml", config);
    f.kestrel()
        .args([
            "serve",
            "--config",
            path_str(&path),
            "--sandbox-driver",
            "fake",
        ])
        .timeout(DEADLINE)
        .assert()
}

/// Starts serve with the fake driver, waits until it is up, stops it with SIGINT and returns
/// (exit success, stderr).
fn serve_until_up(f: &Fixture, config: &str, docker_path: &str) -> (bool, String) {
    let path = f.write("config.toml", config);
    let mut cmd = f.kestrel_std();
    cmd.env("PATH", docker_path).args([
        "serve",
        "--config",
        path_str(&path),
        "--sandbox-driver",
        "fake",
    ]);
    let mut serve = ChildGuard::spawn(cmd);
    let mut lines = serve.stdout_lines();
    lines.until("serve to be running", |l| l.contains("kestrel is running"));
    serve.interrupt();
    let status = serve.wait_exit("serve to stop");
    (status.success(), serve.stderr())
}

#[test]
fn a_valid_advertise_url_is_accepted() {
    for url in [
        "http://kestrel-gateway:7070/",
        "https://gw.example.com/",
        "http://10.0.0.5:9000",
    ] {
        let f = Fixture::new();
        let (log, path) = docker_stub(&f);
        let (ok, stderr) =
            serve_until_up(&f, &config(&format!("advertise_url = \"{url}\"")), &path);
        assert!(ok, "{url}: {stderr}");
        assert!(
            !log.exists(),
            "{url}: docker was called: {:?}",
            std::fs::read_to_string(&log)
        );
    }
}

#[test]
fn a_relative_advertise_url_exits_2_naming_the_key() {
    let f = Fixture::new();
    serve_once(
        &f,
        &config(&format!("advertise_url = \"{SECRET_HOST}/path\"")),
    )
    .code(2)
    .stderr(predicate::str::contains("gateway.advertise_url"))
    .stderr(predicate::str::contains(SECRET_HOST).not());
}

#[test]
fn a_wrong_scheme_exits_2_naming_the_key() {
    for url in [
        format!("ftp://{SECRET_HOST}/"),
        format!("file:///{SECRET_HOST}"),
        format!("ws://{SECRET_HOST}:1/"),
    ] {
        let f = Fixture::new();
        serve_once(&f, &config(&format!("advertise_url = \"{url}\"")))
            .code(2)
            .stderr(predicate::str::contains("gateway.advertise_url"))
            .stderr(predicate::str::contains(SECRET_HOST).not());
    }
}

#[test]
fn an_empty_or_non_string_advertise_url_exits_2_without_echo() {
    for value in ["\"\"", "42"] {
        let f = Fixture::new();
        serve_once(&f, &config(&format!("advertise_url = {value}")))
            .code(2)
            .stderr(predicate::str::contains("advertise_url"));
    }
}

#[test]
fn a_user_in_the_url_is_never_echoed_when_it_is_rejected() {
    let f = Fixture::new();
    serve_once(
        &f,
        &config(&format!(
            "advertise_url = \"gopher://user:{SECRET_HOST}@h/\""
        )),
    )
    .code(2)
    .stderr(predicate::str::contains(SECRET_HOST).not());
}

#[test]
fn a_loopback_listen_prints_no_warning() {
    let f = Fixture::new();
    let (_log, path) = docker_stub(&f);
    let (ok, stderr) = serve_until_up(&f, &config(""), &path);
    assert!(ok, "{stderr}");
    assert!(!stderr.contains("only run tokens protect it"), "{stderr}");
}

#[test]
fn a_non_loopback_listen_prints_the_one_line_warning() {
    for listen in ["0.0.0.0:0", "[::]:0"] {
        let f = Fixture::new();
        let (_log, path) = docker_stub(&f);
        let cfg = format!("[gateway]\nlisten = \"{listen}\"\n{BASE}");
        let (ok, stderr) = serve_until_up(&f, &cfg, &path);
        assert!(ok, "{listen}: {stderr}");
        let warnings: Vec<&str> = stderr
            .lines()
            .filter(|l| l.contains("only run tokens protect it"))
            .collect();
        assert_eq!(warnings.len(), 1, "{listen}: {stderr}");
        let line = warnings[0];
        assert!(line.starts_with("gateway listens on "), "{line}");
        assert!(
            line.ends_with("only run tokens protect it, bind to a private interface if you can"),
            "{line}"
        );
        assert!(
            line.contains(&listen[..listen.len() - 2]),
            "names the address: {line}"
        );
    }
}

#[test]
fn the_fake_driver_never_starts_a_bridge() {
    let f = Fixture::new();
    let (log, path) = docker_stub(&f);
    let (ok, stderr) = serve_until_up(&f, &config(""), &path);
    assert!(ok, "{stderr}");
    assert!(
        !log.exists(),
        "docker was called: {:?}",
        std::fs::read_to_string(&log)
    );
}
