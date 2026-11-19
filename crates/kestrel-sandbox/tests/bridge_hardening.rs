//! Bridge hardening (issue #29): per-database instance, ownership labels, label-scoped stale
//! removal, hardened `docker run` flags, `socat` limits and the digest-pinned image.
//!
//! The exact argv of the happy path is pinned in `tests/bridge.rs`. This file pins the rules
//! behind it. Test vectors: the id of `/tmp/a/kestrel.db` is `b29b95f5` (first 8 hex digits of
//! the 16-digit FNV-1a 64 hash of the path bytes) and that of `/tmp/b/kestrel.db` is `8f7e10f1`.

#![allow(clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{FakeRunner, s};
use kestrel_sandbox::{
    BridgeSpec, CommandOutput, DEFAULT_BRIDGE_IMAGE, GatewayBridge, SandboxError,
};

const DB_A: &str = "/tmp/a/kestrel.db";
const DB_B: &str = "/tmp/b/kestrel.db";
const ID_A: &str = "b29b95f5";
const ID_B: &str = "8f7e10f1";

fn out(text: &str) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: text.to_string(),
        stderr: String::new(),
    }
}

fn fail(stderr: &str) -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

fn is_ps(argv: &[String]) -> bool {
    argv.get(1).map(String::as_str) == Some("ps")
}

fn is_inspect(argv: &[String]) -> bool {
    argv.get(1).map(String::as_str) == Some("inspect")
}

/// Every command succeeds; `ps` answers with `listed`; `inspect` answers with an IP.
fn runner(listed: &'static str) -> Arc<FakeRunner> {
    FakeRunner::with(move |argv| {
        if is_ps(argv) {
            out(listed)
        } else if is_inspect(argv) {
            out("172.18.0.2\n")
        } else {
            out("")
        }
    })
}

async fn start(runner: &Arc<FakeRunner>, db: &str) -> Result<GatewayBridge, SandboxError> {
    GatewayBridge::start(runner.clone(), 7070, BridgeSpec::default(), Path::new(db)).await
}

fn run_argv(runner: &FakeRunner) -> Vec<String> {
    runner
        .runs()
        .into_iter()
        .find(|a| a.get(1).map(String::as_str) == Some("run"))
        .expect("a docker run call")
}

/// The value after `flag`, which must appear exactly once.
fn value_of<'a>(argv: &'a [String], flag: &str) -> &'a str {
    let hits: Vec<usize> = argv
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == flag)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hits.len(), 1, "{flag} appears once in {argv:?}");
    &argv[hits[0] + 1]
}

fn removed_names(runner: &FakeRunner) -> Vec<String> {
    runner
        .runs()
        .into_iter()
        .filter(|a| a.get(1).map(String::as_str) == Some("rm"))
        .map(|a| a.last().cloned().unwrap_or_default())
        .collect()
}

#[test]
fn the_instance_id_is_the_first_8_hex_chars_of_the_fnv1a_hash_of_the_path() {
    assert_eq!(GatewayBridge::instance_id(Path::new(DB_A)), ID_A);
    assert_eq!(GatewayBridge::instance_id(Path::new(DB_B)), ID_B);
}

#[test]
fn the_instance_id_is_stable_lowercase_hex_of_length_8() {
    for db in [DB_A, DB_B, "/", "/very/long/path/with spaces/kestrel.db"] {
        let id = GatewayBridge::instance_id(Path::new(db));
        assert_eq!(id.len(), 8, "{id}");
        assert!(
            id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "{id}"
        );
        assert_eq!(id, GatewayBridge::instance_id(Path::new(db)));
    }
}

#[test]
fn a_relative_path_hashes_like_its_absolute_form() {
    let relative = Path::new("data/kestrel.db");
    let absolute = std::path::absolute(relative).expect("cwd");
    assert_eq!(
        GatewayBridge::instance_id(relative),
        GatewayBridge::instance_id(&absolute)
    );
}

#[test]
fn the_container_name_is_the_prefix_and_the_id() {
    assert_eq!(
        GatewayBridge::container_name(ID_A),
        "kestrel-gateway-b29b95f5"
    );
}

#[tokio::test]
async fn the_container_is_named_and_labelled_after_the_database() {
    for (db, id) in [(DB_A, ID_A), (DB_B, ID_B)] {
        let runner = runner("");
        start(&runner, db).await.expect("bridge starts");
        let argv = run_argv(&runner);
        assert_eq!(value_of(&argv, "--name"), format!("kestrel-gateway-{id}"));
        let labels: Vec<&String> = argv
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--label")
            .map(|(i, _)| &argv[i + 1])
            .collect();
        assert_eq!(
            labels,
            vec!["kestrel.managed=bridge", &format!("kestrel.db={id}")]
        );
    }
}

#[tokio::test]
async fn two_databases_use_two_different_containers() {
    let a = runner("");
    let b = runner("");
    start(&a, DB_A).await.expect("a starts");
    start(&b, DB_B).await.expect("b starts");
    assert_ne!(
        value_of(&run_argv(&a), "--name"),
        value_of(&run_argv(&b), "--name")
    );
}

#[tokio::test]
async fn stale_removal_filters_by_both_labels_for_this_database_only() {
    let runner = runner("");
    start(&runner, DB_B).await.expect("bridge starts");
    assert_eq!(
        runner.runs()[0],
        s(&[
            "docker",
            "ps",
            "-a",
            "--filter",
            "label=kestrel.managed=bridge",
            "--filter",
            &format!("label=kestrel.db={ID_B}"),
            "--format",
            "{{.Names}}",
        ])
    );
}

#[tokio::test]
async fn only_the_listed_names_are_removed_and_blank_lines_are_ignored() {
    let runner = runner("kestrel-gateway-8f7e10f1\r\nkestrel-gateway-8f7e10f1-old\n\n");
    start(&runner, DB_B).await.expect("bridge starts");
    let rms: Vec<Vec<String>> = runner
        .runs()
        .into_iter()
        .filter(|a| a.get(1).map(String::as_str) == Some("rm"))
        .collect();
    assert_eq!(
        rms,
        vec![
            s(&["docker", "rm", "--force", "kestrel-gateway-8f7e10f1"]),
            s(&["docker", "rm", "--force", "kestrel-gateway-8f7e10f1-old"]),
        ]
    );
}

#[tokio::test]
async fn an_unlabelled_container_with_the_old_fixed_name_is_never_removed() {
    // `ps` with both label filters does not list it, so nothing may be removed, and nothing may
    // ever name the old fixed name.
    let runner = runner("");
    start(&runner, DB_A).await.expect("bridge starts");
    assert!(removed_names(&runner).is_empty(), "{:?}", runner.runs());
    for argv in runner.runs() {
        assert!(
            !argv.iter().any(|a| a == "kestrel-gateway"),
            "the old name appears in {argv:?}"
        );
    }
}

#[tokio::test]
async fn a_name_taken_by_an_unlabelled_container_fails_clearly_and_removes_nothing() {
    let runner = FakeRunner::with(|argv| {
        if argv.get(1).map(String::as_str) == Some("run") {
            fail(
                "docker: Error response from daemon: Conflict. The container name \
                 \"/kestrel-gateway-b29b95f5\" is already in use by container \"abc123\".",
            )
        } else if is_inspect(argv) {
            out("172.18.0.2\n")
        } else {
            out("")
        }
    });
    let err = start(&runner, DB_A).await.expect_err("conflict");
    assert!(
        matches!(err, SandboxError::Bridge { step: "run", .. }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("kestrel-gateway-b29b95f5"), "{text}");
    assert!(
        text.contains("kestrel.managed"),
        "explains the missing label: {text}"
    );
    assert!(
        removed_names(&runner).is_empty(),
        "the conflicting container is not ours: {:?}",
        runner.runs()
    );
}

#[tokio::test]
async fn a_failing_ps_is_a_remove_stale_error_and_nothing_else_runs() {
    let runner = FakeRunner::with(|argv| {
        if is_ps(argv) {
            fail("Cannot connect to the Docker daemon")
        } else {
            out("")
        }
    });
    let err = start(&runner, DB_A).await.expect_err("fails");
    assert!(
        matches!(
            err,
            SandboxError::Bridge {
                step: "remove-stale",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("Cannot connect"), "{err}");
    assert_eq!(runner.runs().len(), 1);
}

#[tokio::test]
async fn a_listed_container_that_vanished_is_fine() {
    let runner = FakeRunner::with(|argv| {
        if is_ps(argv) {
            out("kestrel-gateway-b29b95f5\n")
        } else if argv.get(1).map(String::as_str) == Some("rm") && argv.len() == 4 {
            fail("Error: No such container: kestrel-gateway-b29b95f5")
        } else if is_inspect(argv) {
            out("172.18.0.2\n")
        } else {
            out("")
        }
    });
    start(&runner, DB_A).await.expect("bridge starts");
    assert_eq!(removed_names(&runner), vec!["kestrel-gateway-b29b95f5"]);
}

#[tokio::test]
async fn the_container_is_hardened() {
    let runner = runner("");
    start(&runner, DB_A).await.expect("bridge starts");
    let argv = run_argv(&runner);
    assert_eq!(value_of(&argv, "--cap-drop"), "ALL");
    assert_eq!(value_of(&argv, "--security-opt"), "no-new-privileges");
    assert_eq!(value_of(&argv, "--pids-limit"), "64");
    assert_eq!(value_of(&argv, "--memory"), "64m");
    assert_eq!(value_of(&argv, "--user"), "65534:65534");
    assert_eq!(value_of(&argv, "--restart"), "no");
    assert!(argv.iter().any(|a| a == "--read-only"), "{argv:?}");
    for flag in ["-p", "--publish", "-P", "--publish-all", "--privileged"] {
        assert!(!argv.iter().any(|a| a == flag), "{flag} in {argv:?}");
    }
}

#[tokio::test]
async fn every_hardening_flag_comes_before_the_image() {
    let runner = runner("");
    start(&runner, DB_A).await.expect("bridge starts");
    let argv = run_argv(&runner);
    let image = argv
        .iter()
        .position(|a| a == DEFAULT_BRIDGE_IMAGE)
        .expect("image");
    for flag in [
        "--cap-drop",
        "--read-only",
        "--security-opt",
        "--pids-limit",
        "--memory",
        "--user",
        "--label",
    ] {
        let at = argv.iter().position(|a| a == flag).expect(flag);
        assert!(at < image, "{flag} after the image in {argv:?}");
    }
}

#[tokio::test]
async fn socat_gets_an_inactivity_timeout_and_a_child_limit() {
    let runner = runner("");
    start(&runner, DB_A).await.expect("bridge starts");
    let argv = run_argv(&runner);
    let image = argv
        .iter()
        .position(|a| a == DEFAULT_BRIDGE_IMAGE)
        .expect("image");
    assert_eq!(
        argv[image + 1..],
        s(&[
            "-T",
            "300",
            "TCP-LISTEN:7070,fork,reuseaddr,max-children=64",
            "TCP:host.docker.internal:7070",
        ])
    );
}

#[test]
fn the_default_image_is_pinned_by_digest() {
    let (name, digest) = DEFAULT_BRIDGE_IMAGE
        .split_once("@sha256:")
        .expect("name@sha256:digest");
    assert_eq!(name, "alpine/socat:1.8.0.3");
    assert_eq!(digest.len(), 64, "{digest}");
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{digest}");
    assert_eq!(BridgeSpec::default().image, DEFAULT_BRIDGE_IMAGE);
}

#[tokio::test]
async fn stop_removes_only_this_databases_container() {
    let runner = runner("");
    let bridge = start(&runner, DB_B).await.expect("bridge starts");
    bridge.stop().await;
    assert_eq!(
        runner.runs().last(),
        Some(&s(&["docker", "rm", "--force", "kestrel-gateway-8f7e10f1"]))
    );
}
