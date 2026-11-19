//! Issue #3, scenarios 1 to 8. Unit tests with a fake `CommandRunner`; no real processes.

// Shared helpers below are not `#[test]` fns, but a panic is still the right way to fail here.
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{FakeRunner, ID, s, spec};
use futures::StreamExt;
use kestrel_sandbox::{
    AppleContainerDriver, Arch, CommandOutput, GvisorDriver, HostInfo, NetworkPolicy, Os,
    OutputLine, SandboxDriver, SandboxError, SandboxHandle, StdStream, detect,
};
use url::Url;

fn gateway_spec() -> kestrel_sandbox::LaunchSpec {
    let mut spec = spec();
    spec.network = NetworkPolicy::GatewayOnly {
        gateway: Url::parse("http://kestrel-gateway:8080").expect("valid url"),
    };
    spec
}

// Scenario 1: gVisor command line snapshot.

#[tokio::test]
async fn s1_gvisor_create_command_line_snapshot() {
    let runner = FakeRunner::ok();
    let driver = GvisorDriver::new(runner.clone());

    let handle = driver.create(&spec()).await.expect("create");

    assert_eq!(handle, SandboxHandle { id: ID.to_string() });
    assert_eq!(
        runner.runs(),
        vec![s(&[
            "docker",
            "create",
            "--runtime=runsc",
            "--cpus",
            "1.5",
            "--memory",
            "512m",
            "--network",
            "none",
            "--mount",
            "type=bind,source=/tmp/kestrel/ws-1,target=/workspace",
            "--workdir",
            "/workspace",
            "--env",
            "RUST_LOG=info",
            "--env",
            "TASK=review PR 42",
            "alpine:3.20",
            "sh",
            "-c",
            "echo hi",
        ])]
    );
}

// Scenario 2: Apple container command line snapshot.

#[tokio::test]
async fn s2_apple_create_command_line_snapshot() {
    let runner = FakeRunner::ok();
    let driver = AppleContainerDriver::new(runner.clone());

    let handle = driver.create(&spec()).await.expect("create");

    assert_eq!(handle, SandboxHandle { id: ID.to_string() });
    // Apple `container --cpus` takes whole CPUs, so 1500 millicpus rounds up to 2.
    assert_eq!(
        runner.runs(),
        vec![s(&[
            "container",
            "create",
            "--cpus",
            "2",
            "--memory",
            "512M",
            "--network",
            "none",
            "--mount",
            "type=bind,source=/tmp/kestrel/ws-1,target=/workspace",
            "--workdir",
            "/workspace",
            "--env",
            "RUST_LOG=info",
            "--env",
            "TASK=review PR 42",
            "alpine:3.20",
            "sh",
            "-c",
            "echo hi",
        ])]
    );
}

// Scenario 3: argv reaches the runner untouched, one element per argument, no shell.

fn nasty_argv() -> Vec<String> {
    s(&[
        "printf",
        "it's \"quoted\"",
        "two  spaces",
        "$(rm -rf /)",
        "`id`",
        "line one\nline two",
        "; echo pwned",
        "",
    ])
}

async fn assert_argv_untouched(driver: &dyn SandboxDriver, runner: &FakeRunner, program: &str) {
    let mut spec = spec();
    spec.argv = nasty_argv();

    driver.create(&spec).await.expect("create");

    let runs = runner.runs();
    let create = runs.last().expect("one create call");
    assert_eq!(create[0], program, "argv[0] is the CLI, never a shell");
    assert!(
        !create.iter().any(|a| a == "sh" || a == "bash"),
        "no shell wrapper: {create:?}"
    );
    let n = spec.argv.len();
    assert!(create.len() > n);
    assert_eq!(create[create.len() - n - 1], "alpine:3.20");
    assert_eq!(&create[create.len() - n..], spec.argv.as_slice());
}

#[tokio::test]
async fn s3_gvisor_argv_elements_reach_runner_untouched() {
    let runner = FakeRunner::ok();
    let driver = GvisorDriver::new(runner.clone());
    assert_argv_untouched(&driver, &runner, "docker").await;
}

#[tokio::test]
async fn s3_apple_argv_elements_reach_runner_untouched() {
    let runner = FakeRunner::ok();
    let driver = AppleContainerDriver::new(runner.clone());
    assert_argv_untouched(&driver, &runner, "container").await;
}

// Scenario 4: a failing CLI becomes a typed error that carries stderr.

const RUNTIME_ERR: &str =
    "docker: Error response from daemon: unknown or invalid runtime name: runsc.";

fn fail_125() -> Arc<FakeRunner> {
    FakeRunner::with(|_| CommandOutput {
        status: 125,
        stdout: String::new(),
        stderr: format!("{RUNTIME_ERR}\n"),
    })
}

#[tokio::test]
async fn s4_gvisor_create_failure_is_typed_error_with_stderr() {
    let driver = GvisorDriver::new(fail_125());

    let err = driver.create(&spec()).await.expect_err("create must fail");

    match &err {
        SandboxError::CommandFailed { code, stderr, .. } => {
            assert_eq!(*code, 125);
            assert!(stderr.contains(RUNTIME_ERR), "stderr kept: {stderr:?}");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    assert!(
        err.to_string().contains(RUNTIME_ERR),
        "Display shows stderr: {err}"
    );
}

#[tokio::test]
async fn s4_apple_create_failure_is_typed_error_with_stderr() {
    let driver = AppleContainerDriver::new(fail_125());

    let err = driver.create(&spec()).await.expect_err("create must fail");

    match &err {
        SandboxError::CommandFailed { code, stderr, .. } => {
            assert_eq!(*code, 125);
            assert!(stderr.contains(RUNTIME_ERR), "stderr kept: {stderr:?}");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    assert!(
        err.to_string().contains(RUNTIME_ERR),
        "Display shows stderr: {err}"
    );
}

// Scenario 5: stop and destroy are idempotent.

/// Succeeds until `gone` is set, then answers like the CLI does for a removed container.
fn removable(gone: Arc<AtomicBool>, not_found: &'static str) -> Arc<FakeRunner> {
    FakeRunner::with(move |_| {
        if gone.load(Ordering::SeqCst) {
            CommandOutput {
                status: 1,
                stdout: String::new(),
                stderr: format!("{not_found}\n"),
            }
        } else {
            CommandOutput {
                status: 0,
                stdout: format!("{ID}\n"),
                stderr: String::new(),
            }
        }
    })
}

async fn stop_and_destroy_twice(driver: &dyn SandboxDriver, gone: &AtomicBool) {
    let h = SandboxHandle { id: ID.to_string() };
    driver.stop(&h).await.expect("first stop");
    driver.stop(&h).await.expect("second stop");
    driver.destroy(&h).await.expect("first destroy");
    gone.store(true, Ordering::SeqCst);
    driver.destroy(&h).await.expect("second destroy");
    driver.stop(&h).await.expect("stop after destroy");
}

#[tokio::test]
async fn s5_gvisor_stop_and_destroy_twice_is_ok() {
    let gone = Arc::new(AtomicBool::new(false));
    let runner = removable(
        gone.clone(),
        "Error response from daemon: No such container: abc123",
    );
    let driver = GvisorDriver::new(runner);
    stop_and_destroy_twice(&driver, &gone).await;
}

#[tokio::test]
async fn s5_apple_stop_and_destroy_twice_is_ok() {
    let gone = Arc::new(AtomicBool::new(false));
    let runner = removable(
        gone.clone(),
        "Error: notFound: \"container abc123 not found\"",
    );
    let driver = AppleContainerDriver::new(runner);
    stop_and_destroy_twice(&driver, &gone).await;
}

// Scenario 6: start yields lines in order, tagged by stream.

fn interleaved() -> Vec<OutputLine> {
    let line = |stream, text: &str| OutputLine {
        stream,
        line: text.to_string(),
    };
    vec![
        line(StdStream::Stdout, "starting"),
        line(StdStream::Stderr, "warn: low disk"),
        line(StdStream::Stdout, "step 1 done"),
        line(StdStream::Stderr, "error: retrying"),
        line(StdStream::Stdout, "finished"),
    ]
}

async fn start_yields_in_order(driver: &dyn SandboxDriver, runner: &FakeRunner) {
    let h = SandboxHandle { id: ID.to_string() };

    let lines: Vec<OutputLine> = driver.start(&h).await.expect("start").collect().await;

    assert_eq!(lines, interleaved());
    assert!(
        runner
            .spawns()
            .iter()
            .any(|argv| argv.iter().any(|a| a == ID)),
        "start streams the sandbox by id: {:?}",
        runner.spawns()
    );
}

#[tokio::test]
async fn s6_gvisor_start_yields_tagged_lines_in_order() {
    let runner = FakeRunner::streaming(interleaved());
    let driver = GvisorDriver::new(runner.clone());
    start_yields_in_order(&driver, &runner).await;
}

#[tokio::test]
async fn s6_apple_start_yields_tagged_lines_in_order() {
    let runner = FakeRunner::streaming(interleaved());
    let driver = AppleContainerDriver::new(runner.clone());
    start_yields_in_order(&driver, &runner).await;
}

// Scenario 7: detect picks drivers for the host, strongest first.

fn names(drivers: &[Box<dyn SandboxDriver>]) -> Vec<&'static str> {
    drivers.iter().map(|d| d.name()).collect()
}

#[test]
fn s7_detect_linux_with_runsc_gives_gvisor() {
    let host = HostInfo {
        os: Os::Linux,
        arch: Arch::X86_64,
        docker_runtimes: s(&["io.containerd.runc.v2", "runc", "runsc"]),
        apple_container: false,
    };
    let drivers = match detect(&host, false) {
        Ok(d) => d,
        Err(e) => panic!("detect failed: {e}"),
    };
    assert_eq!(names(&drivers), vec!["gvisor"]);
    assert!(drivers[0].is_strong());
}

#[test]
fn s7_detect_macos_26_with_container_gives_apple_container() {
    let host = HostInfo {
        os: Os::MacOs { major: 26 },
        arch: Arch::Aarch64,
        docker_runtimes: Vec::new(),
        apple_container: true,
    };
    let drivers = match detect(&host, false) {
        Ok(d) => d,
        Err(e) => panic!("detect failed: {e}"),
    };
    assert_eq!(names(&drivers), vec!["apple-container"]);
    assert!(drivers[0].is_strong());
}

fn linux_without_runsc() -> HostInfo {
    HostInfo {
        os: Os::Linux,
        arch: Arch::X86_64,
        docker_runtimes: s(&["io.containerd.runc.v2", "runc"]),
        apple_container: false,
    }
}

#[test]
fn s7_detect_linux_without_runsc_strict_is_no_secure_sandbox() {
    let err = match detect(&linux_without_runsc(), false) {
        Ok(d) => panic!("expected NoSecureSandbox, got {:?}", names(&d)),
        Err(e) => e,
    };
    match &err {
        SandboxError::NoSecureSandbox { instructions } => {
            assert!(instructions.contains("runsc"), "{instructions}");
            assert!(
                instructions.contains("https://gvisor.dev"),
                "{instructions}"
            );
        }
        other => panic!("expected NoSecureSandbox, got {other:?}"),
    }
    assert!(err.to_string().contains("https://gvisor.dev"), "{err}");
}

#[test]
fn s7_detect_linux_without_runsc_allow_weak_gives_docker_runc() {
    let drivers = match detect(&linux_without_runsc(), true) {
        Ok(d) => d,
        Err(e) => panic!("detect failed: {e}"),
    };
    assert_eq!(names(&drivers), vec!["docker-runc"]);
    assert!(!drivers[0].is_strong());
}

// Scenario 8: GatewayOnly routes only to the gateway; real secrets never launch.

#[tokio::test]
async fn s8_gvisor_gateway_only_command_line_snapshot() {
    let runner = FakeRunner::ok();
    let driver = GvisorDriver::new(runner.clone());

    driver.create(&gateway_spec()).await.expect("create");

    assert_eq!(
        runner.runs(),
        vec![
            s(&["docker", "network", "create", "--internal", "kestrel-gw"]),
            s(&[
                "docker",
                "create",
                "--runtime=runsc",
                "--cpus",
                "1.5",
                "--memory",
                "512m",
                "--network",
                "kestrel-gw",
                "--mount",
                "type=bind,source=/tmp/kestrel/ws-1,target=/workspace",
                "--workdir",
                "/workspace",
                "--env",
                "RUST_LOG=info",
                "--env",
                "TASK=review PR 42",
                "--env",
                "KESTREL_GATEWAY_URL=http://kestrel-gateway:8080/",
                "alpine:3.20",
                "sh",
                "-c",
                "echo hi",
            ]),
        ]
    );
}

#[tokio::test]
async fn s8_gvisor_gateway_only_tolerates_existing_network() {
    let runner = FakeRunner::with(|argv| {
        if argv.iter().any(|a| a == "network") {
            CommandOutput {
                status: 1,
                stdout: String::new(),
                stderr: "Error response from daemon: network with name kestrel-gw already exists\n"
                    .to_string(),
            }
        } else {
            CommandOutput {
                status: 0,
                stdout: format!("{ID}\n"),
                stderr: String::new(),
            }
        }
    });
    let driver = GvisorDriver::new(runner.clone());

    let h = driver.create(&gateway_spec()).await.expect("create");

    assert_eq!(h.id, ID);
}

#[tokio::test]
async fn s8_apple_gateway_only_command_line_snapshot() {
    let runner = FakeRunner::ok();
    let driver = AppleContainerDriver::new(runner.clone());

    driver.create(&gateway_spec()).await.expect("create");

    let runs = runner.runs();
    assert_eq!(
        runs.last().cloned(),
        Some(s(&[
            "container",
            "create",
            "--cpus",
            "2",
            "--memory",
            "512M",
            "--network",
            "kestrel-gw",
            "--mount",
            "type=bind,source=/tmp/kestrel/ws-1,target=/workspace",
            "--workdir",
            "/workspace",
            "--env",
            "RUST_LOG=info",
            "--env",
            "TASK=review PR 42",
            "--env",
            "KESTREL_GATEWAY_URL=http://kestrel-gateway:8080/",
            "alpine:3.20",
            "sh",
            "-c",
            "echo hi",
        ]))
    );
}

#[tokio::test]
async fn s8_gateway_only_has_no_other_route() {
    for (driver, runner) in [
        {
            let r = FakeRunner::ok();
            (
                Box::new(GvisorDriver::new(r.clone())) as Box<dyn SandboxDriver>,
                r,
            )
        },
        {
            let r = FakeRunner::ok();
            (
                Box::new(AppleContainerDriver::new(r.clone())) as Box<dyn SandboxDriver>,
                r,
            )
        },
    ] {
        driver.create(&gateway_spec()).await.expect("create");
        let runs = runner.runs();
        let create = runs.last().expect("create call");
        for banned in [
            "host",
            "bridge",
            "default",
            "--add-host",
            "--publish",
            "-p",
            "--net",
            "--privileged",
            "--cap-add",
        ] {
            assert!(
                !create
                    .iter()
                    .any(|a| a == banned || a.starts_with(&format!("{banned}="))),
                "{} create line must not contain `{banned}`: {create:?}",
                driver.name()
            );
        }
        let networks: Vec<&String> = create
            .iter()
            .zip(create.iter().skip(1))
            .filter(|(flag, _)| *flag == "--network")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(networks, vec!["kestrel-gw"], "{}", driver.name());
    }
}

fn spec_with_env(key: &str, value: &str) -> kestrel_sandbox::LaunchSpec {
    let mut spec = gateway_spec();
    let mut env = BTreeMap::new();
    env.insert(key.to_string(), value.to_string());
    spec.env = env;
    spec
}

#[tokio::test]
async fn s8_real_api_key_in_env_is_rejected_before_launch() {
    let real = spec_with_env("OPENAI_API_KEY", "sk-proj-4fJx9QmZ2LwT8vRbN3kP7yHcD1sE6gUa");
    for (driver, runner) in [
        {
            let r = FakeRunner::ok();
            (
                Box::new(GvisorDriver::new(r.clone())) as Box<dyn SandboxDriver>,
                r,
            )
        },
        {
            let r = FakeRunner::ok();
            (
                Box::new(AppleContainerDriver::new(r.clone())) as Box<dyn SandboxDriver>,
                r,
            )
        },
    ] {
        let err = driver.create(&real).await.expect_err("must reject");
        match err {
            SandboxError::SecretInEnv { key } => assert_eq!(key, "OPENAI_API_KEY"),
            other => panic!("{}: expected SecretInEnv, got {other:?}", driver.name()),
        }
        assert!(
            runner.all_calls().is_empty(),
            "{}: nothing runs before the check: {:?}",
            driver.name(),
            runner.all_calls()
        );
    }
}

#[tokio::test]
async fn s8_placeholder_api_key_is_not_rejected() {
    // Masked placeholders (architecture 3.6) are not real values.
    let masked = spec_with_env("ANTHROPIC_API_KEY", "[SECRET_3]");
    let runner = FakeRunner::ok();
    let driver = GvisorDriver::new(runner.clone());

    driver
        .create(&masked)
        .await
        .expect("placeholder is allowed");

    let runs = runner.runs();
    let create = runs.last().expect("create call");
    assert!(
        create.iter().any(|a| a == "ANTHROPIC_API_KEY=[SECRET_3]"),
        "{create:?}"
    );
}
