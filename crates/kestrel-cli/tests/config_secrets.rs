//! API keys in the config file never reach the terminal, and a world-readable config is flagged.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{DEADLINE, Fixture, path_str};
use predicates::prelude::*;

/// `kestrel serve --config <file>` with an unknown test driver, so it stops right after the
/// config is read.
fn serve(f: &Fixture, config: &str) -> assert_cmd::assert::Assert {
    let path = f.write("config.toml", config);
    f.kestrel()
        .args([
            "serve",
            "--config",
            path_str(&path),
            "--sandbox-driver",
            "nope",
        ])
        .timeout(DEADLINE)
        .assert()
}

#[test]
fn malformed_key_lines_name_file_and_line_but_not_the_value() {
    for line in [
        "api_key = sk-ant-LEAK1",
        "api_kye = \"sk-ant-LEAK2\"",
        "api_key = 7777777777",
    ] {
        let f = Fixture::new();
        let secret = line.rsplit(['-', ' ']).next().unwrap().trim_matches('"');
        serve(&f, &format!("[providers.anthropic]\n{line}\n"))
            .code(2)
            .stderr(predicate::str::contains("config.toml:2:"))
            .stderr(predicate::str::contains(secret).not());
    }
}

#[cfg(unix)]
#[test]
fn a_world_readable_config_gets_a_warning_without_the_key() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let path = f.write(
        "config.toml",
        "[providers.anthropic]\napi_key = \"sk-ant-LEAK4\"\n",
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    f.kestrel()
        .args([
            "serve",
            "--config",
            path_str(&path),
            "--sandbox-driver",
            "nope",
        ])
        .timeout(DEADLINE)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("warning"))
        .stderr(predicate::str::contains("chmod 600"))
        .stderr(predicate::str::contains("LEAK4").not());

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    f.kestrel()
        .args([
            "serve",
            "--config",
            path_str(&path),
            "--sandbox-driver",
            "nope",
        ])
        .timeout(DEADLINE)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("warning").not());
}

#[test]
fn help_says_what_kestrel_does_and_explains_run_ids() {
    let f = Fixture::new();
    f.kestrel()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Run and watch fleets of AI agents.",
        ));
    for cmd in ["watch", "pause", "resume", "cancel", "budget"] {
        f.kestrel()
            .args([cmd, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "Run id, as printed by `kestrel run`",
            ));
    }
}
