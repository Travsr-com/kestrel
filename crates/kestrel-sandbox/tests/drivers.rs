//! Paths the scenario tests do not reach: docker-runc, `wait`, error mapping, `detect` edge
//! cases, and the real `ProcessRunner` with harmless commands.

mod common;

use std::sync::Arc;

use common::{FakeRunner, ID, s, spec};
use futures::StreamExt;
use kestrel_sandbox::{
    AppleContainerDriver, Arch, CommandOutput, CommandRunner, DockerRuncDriver, ExitStatus,
    GvisorDriver, HostInfo, NetworkPolicy, Os, OutputLine, OutputStream, ProcessRunner,
    SandboxDriver, SandboxError, SandboxHandle, StdStream, detect,
};
use url::Url;

fn handle() -> SandboxHandle {
    SandboxHandle { id: ID.to_string() }
}

#[tokio::test]
async fn docker_runc_uses_runc_and_is_weak() {
    let runner = FakeRunner::ok();
    let driver = DockerRuncDriver::new(runner.clone());
    let mut spec = spec();
    spec.cpu_millis = 2000;
    spec.workspace = None;

    driver.create(&spec).await.expect("create");

    assert_eq!(driver.name(), "docker-runc");
    assert!(!driver.is_strong());
    assert_eq!(
        runner.runs()[0][..9],
        s(&[
            "docker",
            "create",
            "--runtime=runc",
            "--cpus",
            "2",
            "--memory",
            "512m",
            "--network",
            "none"
        ])[..]
    );
    assert!(!runner.runs()[0].iter().any(|a| a == "--mount"));
}

#[tokio::test]
async fn fractional_cpus_keep_only_needed_digits() {
    let runner = FakeRunner::ok();
    let mut spec = spec();
    spec.cpu_millis = 250;
    GvisorDriver::new(runner.clone())
        .create(&spec)
        .await
        .expect("create");
    assert_eq!(runner.runs()[0][4], "0.25");

    let runner = FakeRunner::ok();
    spec.cpu_millis = 0;
    AppleContainerDriver::new(runner.clone())
        .create(&spec)
        .await
        .expect("create");
    assert_eq!(runner.runs()[0][3], "1", "Apple gets at least one CPU");
}

#[tokio::test]
async fn gvisor_wait_parses_exit_code() {
    let runner = FakeRunner::with(|_| CommandOutput {
        status: 0,
        stdout: "3\n".to_string(),
        stderr: String::new(),
    });
    let driver = GvisorDriver::new(runner.clone());

    let status = driver.wait(&handle()).await.expect("wait");

    assert_eq!(status, ExitStatus { code: 3 });
    assert!(!status.success());
    assert_eq!(runner.runs(), vec![s(&["docker", "wait", ID])]);
}

#[tokio::test]
async fn gvisor_wait_rejects_garbage_output() {
    let driver = GvisorDriver::new(FakeRunner::ok());
    let err = driver.wait(&handle()).await.expect_err("not a number");
    assert!(
        err.to_string().contains("unexpected `wait` output"),
        "{err}"
    );
}

#[tokio::test]
async fn apple_wait_is_unsupported() {
    let runner = FakeRunner::ok();
    let driver = AppleContainerDriver::new(runner.clone());
    let err = driver.wait(&handle()).await.expect_err("unsupported");
    assert!(matches!(err, SandboxError::Spawn { .. }), "{err:?}");
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn stop_and_destroy_commands() {
    let runner = FakeRunner::ok();
    let apple = AppleContainerDriver::new(runner.clone());
    apple.stop(&handle()).await.expect("stop");
    apple.destroy(&handle()).await.expect("destroy");
    assert_eq!(
        runner.runs(),
        vec![
            s(&["container", "stop", ID]),
            s(&["container", "delete", "--force", ID])
        ]
    );
    let runner = FakeRunner::ok();
    GvisorDriver::new(runner.clone())
        .destroy(&handle())
        .await
        .expect("destroy");
    assert_eq!(runner.runs(), vec![s(&["docker", "rm", "--force", ID])]);
}

#[tokio::test]
async fn stop_reports_other_failures() {
    let runner = FakeRunner::with(|_| CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: "Cannot connect to the Docker daemon\n".to_string(),
    });
    let err = GvisorDriver::new(runner)
        .stop(&handle())
        .await
        .expect_err("daemon down");
    assert!(matches!(err, SandboxError::CommandFailed { code: 1, .. }));
}

#[tokio::test]
async fn gateway_network_failure_stops_create() {
    let runner = FakeRunner::with(|_| CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: "permission denied\n".to_string(),
    });
    let mut spec = spec();
    spec.network = NetworkPolicy::GatewayOnly {
        gateway: Url::parse("http://gw:1").expect("url"),
    };

    let err = AppleContainerDriver::new(runner.clone())
        .create(&spec)
        .await
        .expect_err("network create failed");

    assert!(err.to_string().contains("permission denied"), "{err}");
    assert_eq!(
        runner.runs(),
        vec![s(&[
            "container",
            "network",
            "create",
            "--internal",
            "kestrel-gw"
        ])]
    );
}

#[tokio::test]
async fn empty_and_placeholder_api_keys_pass() {
    let runner = FakeRunner::ok();
    let mut spec = spec();
    spec.env.insert("A_API_KEY".to_string(), String::new());
    spec.env
        .insert("B_API_KEY".to_string(), "[SECRET_1]".to_string());
    spec.env
        .insert("NOT_A_KEY".to_string(), "sk-live-123".to_string());
    GvisorDriver::new(runner).create(&spec).await.expect("ok");
}

/// A runner whose process can never start.
struct Broken;

#[async_trait::async_trait]
impl CommandRunner for Broken {
    async fn run(&self, _: &[String]) -> std::io::Result<CommandOutput> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no docker",
        ))
    }
    async fn spawn(&self, _: &[String]) -> std::io::Result<OutputStream> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no docker",
        ))
    }
}

#[tokio::test]
async fn runner_io_errors_become_spawn_errors() {
    let driver = GvisorDriver::new(Arc::new(Broken));
    let create = driver.create(&spec()).await.expect_err("no docker");
    assert!(matches!(create, SandboxError::Spawn { .. }), "{create:?}");
    let start = driver.start(&handle()).await.err();
    assert!(matches!(start, Some(SandboxError::Spawn { .. })));
}

fn host(os: Os, arch: Arch, runtimes: &[&str], apple: bool) -> HostInfo {
    HostInfo {
        os,
        arch,
        docker_runtimes: s(runtimes),
        apple_container: apple,
    }
}

fn names(host: &HostInfo, allow_weak: bool) -> Vec<&'static str> {
    match detect(host, allow_weak) {
        Ok(d) => d.iter().map(|d| d.name()).collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn detect_edge_cases() {
    let linux = host(Os::Linux, Arch::Aarch64, &["runc", "runsc"], false);
    assert_eq!(names(&linux, true), vec!["gvisor", "docker-runc"]);
    // Too old, Intel, or no CLI: no Apple driver.
    for h in [
        host(Os::MacOs { major: 15 }, Arch::Aarch64, &[], true),
        host(Os::MacOs { major: 26 }, Arch::X86_64, &[], true),
        host(Os::MacOs { major: 26 }, Arch::Aarch64, &[], false),
        host(Os::Windows, Arch::X86_64, &[], false),
    ] {
        assert!(names(&h, true).is_empty(), "{h:?}");
    }
    // Weak allowed but docker is not installed.
    let err = detect(&host(Os::Other, Arch::Other, &[], false), true).err();
    assert!(matches!(err, Some(SandboxError::NoSecureSandbox { .. })));
}

#[tokio::test]
async fn process_runner_run_captures_output_and_status() {
    let out = ProcessRunner.run(&s(&["echo", "hi"])).await.expect("echo");
    assert_eq!(
        out,
        CommandOutput {
            status: 0,
            stdout: "hi\n".to_string(),
            stderr: String::new()
        }
    );
    let out = ProcessRunner.run(&s(&["false"])).await.expect("false");
    assert_ne!(out.status, 0);
}

#[tokio::test]
async fn process_runner_rejects_empty_and_missing_programs() {
    assert!(ProcessRunner.run(&[]).await.is_err());
    assert!(ProcessRunner.spawn(&[]).await.is_err());
    let missing = s(&["kestrel-no-such-program"]);
    assert!(ProcessRunner.run(&missing).await.is_err());
}

#[tokio::test]
async fn process_runner_spawn_tags_lines_by_stream() {
    // A shell is fine here: this is a test, not the driver.
    let argv = s(&["sh", "-c", "echo out; echo err >&2"]);
    let lines: Vec<OutputLine> = ProcessRunner
        .spawn(&argv)
        .await
        .expect("spawn")
        .collect()
        .await;
    let line = |stream, text: &str| OutputLine {
        stream,
        line: text.to_string(),
    };
    assert!(lines.contains(&line(StdStream::Stdout, "out")), "{lines:?}");
    assert!(lines.contains(&line(StdStream::Stderr, "err")), "{lines:?}");
    assert_eq!(lines.len(), 2);
}
