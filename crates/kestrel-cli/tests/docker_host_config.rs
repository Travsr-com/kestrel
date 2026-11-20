//! `gateway.docker_host` (issue #53), observable from outside: the accepted values, the usage
//! error for anything else (exit 2, names the key, never echoes the value) and "the fake driver
//! never starts a bridge, so no mode is chosen or printed".
//!
//! The mode choice itself (bind probing, bind rules, the printed line, the bridge argv) is
//! covered by the unit tests in `src/serve.rs` and `kestrel-sandbox/tests/bridge_vm.rs`, so
//! these tests need no docker daemon.
//!
//! ```toml
//! [gateway]
//! docker_host = "auto"   # or "native", or "vm"; default "auto"
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{ChildGuard, DEADLINE, Fixture, path_str};
use predicates::prelude::*;

const BASE: &str = "[providers.anthropic]\napi_key = \"test-key\"\n";
const SECRET: &str = "secret-mode-9f3a";

fn config(gateway: &str) -> String {
    format!("[gateway]\nlisten = \"127.0.0.1:0\"\n{gateway}\n{BASE}")
}

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

/// Serves with the fake driver until it is up, stops it with SIGINT and returns
/// (exit success, stderr).
fn serve_until_up(f: &Fixture, config: &str) -> (bool, String) {
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
    let mut lines = serve.stdout_lines();
    lines.until("serve to be running", |l| l.contains("kestrel is running"));
    serve.interrupt();
    let status = serve.wait_exit("serve to stop");
    (status.success(), serve.stderr())
}

#[test]
fn auto_native_and_vm_are_accepted_and_the_key_may_be_left_out() {
    for gateway in [
        "",
        "docker_host = \"auto\"",
        "docker_host = \"native\"",
        "docker_host = \"vm\"",
    ] {
        let f = Fixture::new();
        let (ok, stderr) = serve_until_up(&f, &config(gateway));
        assert!(ok, "{gateway}: {stderr}");
        assert!(
            !stderr.contains("gateway: docker daemon"),
            "no bridge, so no mode line: {stderr}"
        );
    }
}

#[test]
fn any_other_docker_host_exits_2_naming_the_key_without_echoing_the_value() {
    for value in [
        format!("\"{SECRET}\""),
        "\"VM\"".to_string(),
        "\"\"".to_string(),
        "42".to_string(),
        "true".to_string(),
        format!("[\"{SECRET}\"]"),
    ] {
        let f = Fixture::new();
        serve_once(&f, &config(&format!("docker_host = {value}")))
            .code(2)
            .stderr(predicate::str::contains("gateway.docker_host"))
            .stderr(predicate::str::contains(SECRET).not());
    }
}

#[test]
fn a_misspelled_key_is_rejected_without_echoing_the_typo() {
    let f = Fixture::new();
    serve_once(&f, &config(&format!("docker_hots_{SECRET} = \"vm\"")))
        .code(2)
        .stderr(predicate::str::contains(SECRET).not());
}

#[test]
fn a_duplicate_docker_host_key_is_a_toml_error_that_may_name_the_key() {
    let f = Fixture::new();
    serve_once(
        &f,
        &config("docker_host = \"vm\"\ndocker_host = \"native\""),
    )
    .code(2)
    .stderr(predicate::str::contains("docker_host"));
}
