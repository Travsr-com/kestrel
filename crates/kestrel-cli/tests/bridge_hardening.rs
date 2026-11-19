//! `kestrel serve` and bridge hardening (issue #29), observable from outside: one serve per
//! database (an exclusive lock file next to it), independent serves for different databases and
//! the `gateway.port` key.
//!
//! The docker side (bind address discovery, bridge names) is covered by the unit tests in
//! `src/serve.rs` and `kestrel-sandbox/tests/bridge_hardening.rs`: the debug-only fake driver
//! never starts a bridge, and these tests must not need a docker daemon.
//!
//! Lock file: `<db>.serve.lock`, for `--db /x/kestrel.db` that is `/x/kestrel.db.serve.lock`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{ChildGuard, DEADLINE, Fixture, path_str};
use predicates::prelude::*;

const BASE: &str = "[providers.anthropic]\napi_key = \"test-key\"\n";

fn config(gateway: &str) -> String {
    format!("[gateway]\nlisten = \"127.0.0.1:0\"\n{gateway}\n{BASE}")
}

fn lock_file(f: &Fixture) -> String {
    format!("{}.serve.lock", f.db.display())
}

/// Starts `kestrel serve` with the fake driver and waits until it is up.
fn start_serve(f: &Fixture, config: &str) -> ChildGuard {
    let path = f.write("config.toml", config);
    let mut cmd = f.kestrel_std();
    cmd.args([
        "serve",
        "--config",
        path_str(&path),
        "--sandbox-driver",
        "fake",
    ]);
    let mut serve = ChildGuard::spawn(cmd);
    serve
        .stdout_lines()
        .until("serve to be running", |l| l.contains("kestrel is running"));
    serve
}

fn stop(mut serve: ChildGuard) -> bool {
    serve.interrupt();
    serve.wait_exit("serve to stop").success()
}

#[test]
fn serve_creates_the_lock_file_next_to_the_database() {
    let f = Fixture::new();
    let serve = start_serve(&f, &config(""));
    assert!(std::path::Path::new(&lock_file(&f)).exists());
    assert!(stop(serve));
}

#[test]
fn a_second_serve_on_the_same_database_exits_1_naming_the_lock_file() {
    let f = Fixture::new();
    let first = start_serve(&f, &config(""));
    let path = f.write("second.toml", &config(""));
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
        .code(1)
        .stderr(predicate::str::contains(lock_file(&f)));
    assert!(
        stop(first),
        "the first serve is unaffected and stops cleanly"
    );
}

#[test]
fn the_lock_is_released_when_serve_stops() {
    let f = Fixture::new();
    assert!(stop(start_serve(&f, &config(""))));
    assert!(stop(start_serve(&f, &config(""))));
}

#[test]
fn serves_on_different_databases_run_side_by_side() {
    let a = Fixture::new();
    let b = Fixture::new();
    let first = start_serve(&a, &config(""));
    let second = start_serve(&b, &config(""));
    assert!(stop(first));
    assert!(stop(second));
}

#[test]
fn gateway_port_is_accepted_and_an_explicit_listen_wins() {
    let f = Fixture::new();
    let serve = start_serve(&f, &config("port = 9000"));
    assert!(stop(serve));
}

#[test]
fn a_bad_gateway_port_exits_2_naming_the_key() {
    for bad in ["0", "70000", "\"7070\""] {
        let f = Fixture::new();
        let path = f.write("config.toml", &config(&format!("port = {bad}")));
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
            .code(2)
            .stderr(predicate::str::contains("port"));
    }
}
