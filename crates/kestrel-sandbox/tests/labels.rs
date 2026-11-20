//! Issue #19: every sandbox carries a `kestrel.run=<run id>` label, and a driver can list the
//! sandboxes that carry the label (`leftovers`), so a crashed engine's containers are found.
//!
//! Docker-style drivers: `docker create --label k=v` and
//! `docker ps -a --filter label=kestrel.run --format {{.ID}}`.
//!
//! Apple `container`: `container create --label <key=val>` exists, but `container list` has no
//! label filter and no per-row label format (`--format` takes json, table, yaml or toml, `-q`
//! prints ids only), so a label sweep would need JSON parsing of every container. Until then the
//! Apple driver keeps the default `leftovers` (an empty list, no process spawned).
//! See https://github.com/apple/container/blob/main/docs/command-reference.md (`container
//! create`, `container list`).

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{FakeRunner, s, spec};
use kestrel_sandbox::{
    AppleContainerDriver, CommandOutput, DockerRuncDriver, GvisorDriver, LaunchSpec, RUN_LABEL,
    SandboxDriver, SandboxError, SandboxHandle,
};

const RUN: &str = "0198c5a2-7b1e-7c3d-9f00-1a2b3c4d5e6f";

fn labelled() -> LaunchSpec {
    let mut spec = spec();
    spec.labels.insert(RUN_LABEL.to_string(), RUN.to_string());
    spec
}

/// The argv of the one `create` call.
fn create_argv(runner: &FakeRunner) -> Vec<String> {
    runner
        .runs()
        .into_iter()
        .find(|a| a.get(1).map(String::as_str) == Some("create") && !a.contains(&"network".into()))
        .expect("a create call")
}

/// Index of `--label <value>` as an adjacent pair.
fn label_at(argv: &[String], value: &str) -> Option<usize> {
    argv.windows(2)
        .position(|w| w[0] == "--label" && w[1] == value)
}

fn assert_label_before_image(argv: &[String]) {
    let at = label_at(argv, &format!("kestrel.run={RUN}"))
        .unwrap_or_else(|| panic!("no `--label kestrel.run={RUN}` in {argv:?}"));
    let image = argv.iter().position(|a| a == "alpine:3.20").expect("image");
    assert!(at + 1 < image, "the label is a create option: {argv:?}");
}

#[test]
fn s1_run_label_key_is_kestrel_run() {
    assert_eq!(RUN_LABEL, "kestrel.run");
}

#[tokio::test]
async fn s1_gvisor_create_passes_the_run_label() {
    let runner = FakeRunner::ok();
    GvisorDriver::new(runner.clone())
        .create(&labelled())
        .await
        .expect("create");
    assert_label_before_image(&create_argv(&runner));
}

#[tokio::test]
async fn s1_docker_runc_create_passes_the_run_label() {
    let runner = FakeRunner::ok();
    DockerRuncDriver::new(runner.clone())
        .create(&labelled())
        .await
        .expect("create");
    assert_label_before_image(&create_argv(&runner));
}

#[tokio::test]
async fn s1_apple_create_passes_the_run_label() {
    let runner = FakeRunner::ok();
    AppleContainerDriver::new(runner.clone())
        .create(&labelled())
        .await
        .expect("create");
    assert_label_before_image(&create_argv(&runner));
}

#[tokio::test]
async fn s1_every_label_gets_its_own_flag_in_key_order() {
    let runner = FakeRunner::ok();
    let mut spec = labelled();
    spec.labels.insert("a.first".into(), "1".into());
    spec.labels
        .insert("z.last".into(), "has space; $(id)".into());
    GvisorDriver::new(runner.clone())
        .create(&spec)
        .await
        .expect("create");
    let argv = create_argv(&runner);
    let first = label_at(&argv, "a.first=1").expect("first label");
    let run = label_at(&argv, &format!("kestrel.run={RUN}")).expect("run label");
    let last = label_at(&argv, "z.last=has space; $(id)").expect("value is one argv element");
    assert!(first < run && run < last, "BTreeMap order: {argv:?}");
}

#[tokio::test]
async fn s1_no_labels_means_no_label_flag() {
    let runner = FakeRunner::ok();
    GvisorDriver::new(runner.clone())
        .create(&spec())
        .await
        .expect("create");
    assert!(!create_argv(&runner).iter().any(|a| a == "--label"));
}

// ---------------------------------------------------------------------------------------------
// leftovers

fn ps_output(stdout: &str) -> Arc<FakeRunner> {
    let stdout = stdout.to_string();
    FakeRunner::with(move |_| CommandOutput {
        status: 0,
        stdout: stdout.clone(),
        stderr: String::new(),
    })
}

fn ps_argv() -> Vec<String> {
    s(&[
        "docker",
        "ps",
        "-a",
        "--filter",
        "label=kestrel.run",
        "--format",
        "{{.ID}}",
    ])
}

fn handles(ids: &[&str]) -> Vec<SandboxHandle> {
    ids.iter()
        .map(|id| SandboxHandle { id: (*id).into() })
        .collect()
}

#[tokio::test]
async fn s1_gvisor_leftovers_lists_labelled_containers_with_docker_ps() {
    let runner = ps_output("abc123\ndef456\n");
    let found = GvisorDriver::new(runner.clone())
        .leftovers()
        .await
        .expect("leftovers");
    assert_eq!(found, handles(&["abc123", "def456"]));
    assert_eq!(runner.all_calls(), vec![ps_argv()]);
}

#[tokio::test]
async fn s1_docker_runc_leftovers_uses_the_same_command() {
    let runner = ps_output("abc123\n");
    let found = DockerRuncDriver::new(runner.clone())
        .leftovers()
        .await
        .expect("leftovers");
    assert_eq!(found, handles(&["abc123"]));
    assert_eq!(runner.all_calls(), vec![ps_argv()]);
}

#[tokio::test]
async fn s1_leftovers_with_nothing_labelled_is_empty() {
    for out in ["", "\n", "  \n\n"] {
        let found = GvisorDriver::new(ps_output(out))
            .leftovers()
            .await
            .expect("leftovers");
        assert!(found.is_empty(), "{out:?} gave {found:?}");
    }
}

#[tokio::test]
async fn s1_invalid_ids_from_docker_ps_are_dropped_and_never_reach_the_runner() {
    let bad = ["-rf", "$(evil)", "a b", "x;rm", "--all", "a/b", "ünï"];
    let out = format!("good-1\n{}\nok_2.x\r\n", bad.join("\n"));
    let runner = ps_output(&out);
    let driver = GvisorDriver::new(runner.clone());

    let found = driver.leftovers().await.expect("leftovers");

    assert_eq!(found, handles(&["good-1", "ok_2.x"]));
    // Using what came back (as the engine does) only ever puts valid ids in an argv.
    for h in &found {
        driver.destroy(h).await.expect("destroy");
    }
    for call in runner.all_calls() {
        for b in bad {
            assert!(
                !call.iter().any(|a| a.contains(b)),
                "{b:?} reached {call:?}"
            );
        }
    }
}

#[tokio::test]
async fn s1_leftovers_failure_is_a_typed_error_with_stderr() {
    let runner = FakeRunner::with(|_| CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: "Cannot connect to the Docker daemon\n".into(),
    });
    let err = GvisorDriver::new(runner)
        .leftovers()
        .await
        .expect_err("ps failed");
    match err {
        SandboxError::CommandFailed { code, stderr, .. } => {
            assert_eq!(code, 1);
            assert!(stderr.contains("Cannot connect"), "{stderr:?}");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn s1_apple_leftovers_is_empty_and_runs_no_process() {
    let runner = FakeRunner::ok();
    let found = AppleContainerDriver::new(runner.clone())
        .leftovers()
        .await
        .expect("leftovers");
    assert!(found.is_empty());
    assert!(runner.all_calls().is_empty(), "no label filter to ask for");
}
