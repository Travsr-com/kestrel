//! Scenario 1: `kestrel --help` lists every command, and the help stays short and plain.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use assert_cmd::Command;
use predicates::prelude::*;

const COMMANDS: [&str; 9] = [
    "serve", "run", "watch", "ps", "approve", "pause", "resume", "cancel", "budget",
];

fn kestrel() -> Command {
    Command::cargo_bin("kestrel").expect("kestrel binary")
}

#[test]
fn s1_help_lists_every_command() {
    let out = kestrel().arg("--help").assert().success();
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    for cmd in COMMANDS {
        assert!(
            text.lines().any(|l| l.trim_start().starts_with(cmd)),
            "`{cmd}` missing from --help:\n{text}"
        );
    }
}

#[test]
fn s1_help_is_short_and_plain() {
    let out = kestrel().arg("--help").assert().success();
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(text.lines().count() <= 30, "help too long:\n{text}");
    assert!(
        !text.contains('\u{2014}') && !text.contains('\u{2013}'),
        "dash in help"
    );
}

#[test]
fn s1_every_command_has_json_and_db() {
    for cmd in COMMANDS {
        kestrel()
            .args([cmd, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--json"))
            .stdout(predicate::str::contains("--db"));
    }
}

#[test]
fn s1_serve_help_hides_the_test_driver_flag() {
    kestrel()
        .args(["serve", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"))
        .stdout(predicate::str::contains("sandbox-driver").not());
}

#[test]
fn s1_no_arguments_prints_usage_and_exits_2() {
    kestrel()
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn s1_unknown_command_exits_2() {
    kestrel().arg("frobnicate").assert().code(2);
}
