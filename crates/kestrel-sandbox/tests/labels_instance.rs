//! Issue #19, instance label: with an instance id a driver lists only the sandboxes of that
//! instance, so two Kestrel instances on one Docker host do not destroy each other's.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{FakeRunner, s, spec};
use kestrel_sandbox::{
    AppleContainerDriver, Arch, CommandOutput, DockerRuncDriver, GvisorDriver, HostInfo,
    INSTANCE_LABEL, Os, SandboxDriver, SandboxError, SandboxHandle, detect_for,
};

fn ps_output(stdout: &str) -> Arc<FakeRunner> {
    let stdout = stdout.to_string();
    FakeRunner::with(move |_| CommandOutput {
        status: 0,
        stdout: stdout.clone(),
        stderr: String::new(),
    })
}

fn ps_argv(instance: &str) -> Vec<String> {
    s(&[
        "docker",
        "ps",
        "-a",
        "--filter",
        "label=kestrel.run",
        "--filter",
        &format!("label=kestrel.instance={instance}"),
        "--format",
        "{{.ID}}",
    ])
}

#[test]
fn the_instance_label_key_is_kestrel_instance() {
    assert_eq!(INSTANCE_LABEL, "kestrel.instance");
}

#[tokio::test]
async fn gvisor_leftovers_adds_the_instance_filter() {
    let runner = ps_output("abc123\n");
    let found = GvisorDriver::new(runner.clone())
        .with_instance("1a2b3c4d")
        .leftovers()
        .await
        .expect("leftovers");
    assert_eq!(
        found,
        vec![SandboxHandle {
            id: "abc123".into()
        }]
    );
    assert_eq!(runner.all_calls(), vec![ps_argv("1a2b3c4d")]);
}

#[tokio::test]
async fn docker_runc_leftovers_adds_the_instance_filter() {
    let runner = ps_output("");
    DockerRuncDriver::new(runner.clone())
        .with_instance("my.inst_1-x")
        .leftovers()
        .await
        .expect("leftovers");
    assert_eq!(runner.all_calls(), vec![ps_argv("my.inst_1-x")]);
}

#[tokio::test]
async fn without_an_instance_the_filter_is_absent() {
    let runner = ps_output("");
    GvisorDriver::new(runner.clone())
        .leftovers()
        .await
        .expect("leftovers");
    let calls = runner.all_calls();
    assert!(!calls[0].iter().any(|a| a.contains("kestrel.instance")));
}

#[tokio::test]
async fn an_unsafe_instance_id_is_refused_and_runs_no_process() {
    for bad in ["", "-x", "a b", "x;rm", "a=b", "$(id)", "ünï"] {
        let runner = ps_output("abc123\n");
        let err = GvisorDriver::new(runner.clone())
            .with_instance(bad)
            .leftovers()
            .await
            .expect_err("invalid instance id");
        assert!(matches!(err, SandboxError::InvalidId { .. }), "{bad:?}");
        assert!(runner.all_calls().is_empty(), "{bad:?} reached the runner");
    }
}

#[tokio::test]
async fn apple_leftovers_stays_empty_with_an_instance() {
    let runner = FakeRunner::ok();
    let found = AppleContainerDriver::new(runner.clone())
        .with_instance("1a2b3c4d")
        .leftovers()
        .await
        .expect("leftovers");
    assert!(found.is_empty());
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn a_label_set_by_the_engine_for_the_instance_is_passed_on_create() {
    let runner = FakeRunner::ok();
    let mut spec = spec();
    spec.labels
        .insert(INSTANCE_LABEL.to_string(), "1a2b3c4d".to_string());
    GvisorDriver::new(runner.clone())
        .with_instance("1a2b3c4d")
        .create(&spec)
        .await
        .expect("create");
    let argv = runner
        .runs()
        .into_iter()
        .find(|a| a.get(1).map(String::as_str) == Some("create") && !a.contains(&"network".into()))
        .expect("a create call");
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--label" && w[1] == "kestrel.instance=1a2b3c4d"),
        "{argv:?}"
    );
}

#[test]
fn detect_for_builds_the_same_drivers_as_detect_with_an_instance() {
    let host = HostInfo {
        os: Os::Linux,
        arch: Arch::X86_64,
        docker_runtimes: vec!["runc".into(), "runsc".into()],
        apple_container: false,
    };
    let names = |instance| {
        detect_for(&host, true, instance)
            .expect("drivers")
            .iter()
            .map(|d| d.name())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(Some("1a2b3c4d")), vec!["gvisor", "docker-runc"]);
    assert_eq!(names(None), names(Some("1a2b3c4d")));

    let mac = HostInfo {
        os: Os::MacOs { major: 26 },
        arch: Arch::Aarch64,
        docker_runtimes: vec![],
        apple_container: true,
    };
    let drivers = detect_for(&mac, false, Some("1a2b3c4d")).expect("drivers");
    assert_eq!(drivers[0].name(), "apple-container");
}
