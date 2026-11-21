//! `sandbox.allowed_roots` (issue #15), observable from outside: absolute paths are accepted
//! (the key may also be left out), anything else exits 2 naming the key without echoing the
//! value. What the roots enforce is covered in `kestrel-sandbox/tests/roots.rs`.
//!
//! ```toml
//! [sandbox]
//! allowed_roots = ["/data/kestrel/workspaces"]   # default: <db dir>/workspaces
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{ChildGuard, DEADLINE, Fixture, path_str};
use predicates::prelude::*;

const BASE: &str = "[providers.anthropic]\napi_key = \"test-key\"\n";
const SECRET: &str = "secret-root-9f3a";

fn config(sandbox: &str) -> String {
    format!("[gateway]\nlisten = \"127.0.0.1:0\"\n[sandbox]\n{sandbox}\n{BASE}")
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

#[test]
fn absolute_roots_and_a_missing_key_are_accepted() {
    let f = Fixture::new();
    for sandbox in [
        "",
        "allowed_roots = [\"/tmp\"]",
        "allowed_roots = [\"/tmp\", \"/var\"]",
    ] {
        let path = f.write("config.toml", &config(sandbox));
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
        assert!(status.success(), "{sandbox}: {}", serve.stderr());
    }
}

#[test]
fn bad_allowed_roots_exit_2_naming_the_key_without_echoing_the_value() {
    for value in [
        format!("\"/{SECRET}\""),
        format!("[\"{SECRET}/relative\"]"),
        format!("[\"/ok\", \"{SECRET}\"]"),
        format!("[[\"/{SECRET}\"]]"),
        "[]".to_string(),
        "[42]".to_string(),
        "42".to_string(),
        "true".to_string(),
    ] {
        let f = Fixture::new();
        serve_once(&f, &config(&format!("allowed_roots = {value}")))
            .code(2)
            .stderr(predicate::str::contains("sandbox.allowed_roots"))
            .stderr(predicate::str::contains(SECRET).not());
    }
}

#[test]
fn a_misspelled_sandbox_key_is_rejected_without_echoing_the_typo() {
    let f = Fixture::new();
    serve_once(&f, &config(&format!("allowed_roots_{SECRET} = [\"/tmp\"]")))
        .code(2)
        .stderr(predicate::str::contains(SECRET).not());
}
