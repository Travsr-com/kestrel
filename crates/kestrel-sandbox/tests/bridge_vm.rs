//! The bridge when docker runs in a VM (issue #53). The VM already resolves
//! `host.docker.internal` to the host, so the bridge container must NOT be started with
//! `--add-host host.docker.internal:host-gateway` (that would point the name at the VM's own
//! bridge and the connection would be refused). Nothing else about the container changes.
//!
//! `BridgeSpec::add_host_gateway` is true by default (docker on the same machine, the pinned
//! argv of `tests/bridge.rs`) and false for a VM.

#![allow(clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{FakeRunner, s};
use kestrel_sandbox::{BridgeSpec, CommandOutput, GatewayBridge};

const DB: &str = "/tmp/a/kestrel.db";
const ADD_HOST: &str = "--add-host";
const HOST_GATEWAY: &str = "host.docker.internal:host-gateway";

fn out(text: &str) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: text.to_string(),
        stderr: String::new(),
    }
}

fn runner() -> Arc<FakeRunner> {
    FakeRunner::with(|argv| match argv.get(1).map(String::as_str) {
        Some("ps") => out(""),
        Some("inspect") => out("172.18.0.2\n"),
        _ => out(""),
    })
}

fn vm() -> BridgeSpec {
    BridgeSpec {
        add_host_gateway: false,
        ..BridgeSpec::default()
    }
}

async fn start(spec: BridgeSpec) -> Arc<FakeRunner> {
    let runner = runner();
    GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
        .await
        .expect("bridge starts");
    runner
}

fn run_argv(runner: &FakeRunner) -> Vec<String> {
    runner
        .runs()
        .into_iter()
        .find(|a| a.get(1).map(String::as_str) == Some("run"))
        .expect("a docker run call")
}

#[test]
fn the_default_spec_adds_the_host_gateway_alias() {
    assert!(BridgeSpec::default().add_host_gateway);
}

#[tokio::test]
async fn native_keeps_the_add_host_flag_and_value_adjacent() {
    let argv = run_argv(&*start(BridgeSpec::default()).await);
    let at = argv.iter().position(|a| a == ADD_HOST).expect("--add-host");
    assert_eq!(argv[at + 1], HOST_GATEWAY);
    assert_eq!(argv.iter().filter(|a| *a == ADD_HOST).count(), 1);
}

#[tokio::test]
async fn an_explicit_true_is_the_same_as_the_default() {
    let explicit = BridgeSpec {
        add_host_gateway: true,
        ..BridgeSpec::default()
    };
    let a = start(explicit).await;
    let b = start(BridgeSpec::default()).await;
    assert_eq!(a.runs(), b.runs());
}

#[tokio::test]
async fn vm_has_neither_the_add_host_flag_nor_its_value() {
    let argv = run_argv(&*start(vm()).await);
    assert!(!argv.iter().any(|a| a == ADD_HOST), "{argv:?}");
    assert!(!argv.iter().any(|a| a == HOST_GATEWAY), "{argv:?}");
    assert!(!argv.iter().any(|a| a.contains("host-gateway")), "{argv:?}");
}

#[tokio::test]
async fn vm_run_is_the_native_run_minus_exactly_the_two_add_host_entries() {
    let native = run_argv(&*start(BridgeSpec::default()).await);
    let mut expected = native.clone();
    let at = expected.iter().position(|a| a == ADD_HOST).expect("flag");
    expected.drain(at..=at + 1);
    assert_eq!(run_argv(&*start(vm()).await), expected);
}

#[tokio::test]
async fn vm_run_argv_is_pinned_in_full() {
    let argv = run_argv(&*start(vm()).await);
    assert_eq!(
        argv,
        s(&[
            "docker",
            "run",
            "--detach",
            "--name",
            "kestrel-gateway-b29b95f5",
            "--label",
            "kestrel.managed=bridge",
            "--label",
            "kestrel.db=b29b95f5",
            "--network",
            "bridge",
            "--restart",
            "no",
            "--cap-drop",
            "ALL",
            "--read-only",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            "536",
            "--memory",
            "560m",
            "--user",
            "65534:65534",
            kestrel_sandbox::DEFAULT_BRIDGE_IMAGE,
            "-T",
            "300",
            "TCP-LISTEN:7070,fork,reuseaddr,max-children=528",
            "TCP:host.docker.internal:7070",
        ])
    );
}

#[tokio::test]
async fn vm_still_forwards_to_host_docker_internal() {
    let argv = run_argv(&*start(vm()).await);
    assert_eq!(
        argv.last().map(String::as_str),
        Some("TCP:host.docker.internal:7070")
    );
}

#[tokio::test]
async fn every_other_step_is_identical_in_both_modes() {
    let not_run = |r: &FakeRunner| -> Vec<Vec<String>> {
        r.runs()
            .into_iter()
            .filter(|a| a.get(1).map(String::as_str) != Some("run"))
            .collect()
    };
    let native = start(BridgeSpec::default()).await;
    let vm = start(vm()).await;
    assert_eq!(not_run(&native), not_run(&vm));
    assert_eq!(native.runs().len(), vm.runs().len());
}

#[tokio::test]
async fn a_failing_vm_run_is_cleaned_up_like_a_native_one() {
    let runner = FakeRunner::with(|argv| {
        if argv.get(1).map(String::as_str) == Some("run") {
            CommandOutput {
                status: 1,
                stdout: String::new(),
                stderr: "boom".to_string(),
            }
        } else {
            out("")
        }
    });
    let err = GatewayBridge::start(runner.clone(), 7070, vm(), Path::new(DB))
        .await
        .expect_err("run fails");
    assert!(err.to_string().contains("boom"), "{err}");
    let last = runner.runs().pop().expect("a call");
    assert_eq!(
        last,
        s(&["docker", "rm", "--force", "kestrel-gateway-b29b95f5"])
    );
}
