//! `HostInfo::probe`: what the host has, read through a fake `CommandRunner`.

mod common;

use common::FakeRunner;
use kestrel_sandbox::{Arch, CommandOutput, HostInfo, Os};

fn out(status: i32, stdout: &str) -> CommandOutput {
    CommandOutput {
        status,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

#[tokio::test]
async fn linux_host_lists_docker_runtimes() {
    let runner = FakeRunner::with(|argv| match argv[0].as_str() {
        "docker" => out(0, "io.containerd.runc.v2 runc runsc \n"),
        _ => out(127, ""),
    });
    let host = HostInfo::probe(runner.as_ref(), "linux", "x86_64").await;
    assert_eq!(
        host,
        HostInfo {
            os: Os::Linux,
            arch: Arch::X86_64,
            docker_runtimes: vec![
                "io.containerd.runc.v2".into(),
                "runc".into(),
                "runsc".into()
            ],
            apple_container: false,
        }
    );
    assert!(runner.runs().iter().all(|a| a[0] != "sw_vers"));
}

#[tokio::test]
async fn macos_host_reads_its_major_version_and_apple_container() {
    let runner = FakeRunner::with(|argv| match argv[0].as_str() {
        "sw_vers" => out(0, "26.1\n"),
        "container" => out(0, "container CLI version 0.5.0\n"),
        _ => out(1, ""),
    });
    let host = HostInfo::probe(runner.as_ref(), "macos", "aarch64").await;
    assert_eq!(host.os, Os::MacOs { major: 26 });
    assert_eq!(host.arch, Arch::Aarch64);
    assert!(host.apple_container);
    assert!(host.docker_runtimes.is_empty());
}

#[tokio::test]
async fn unreadable_macos_version_is_zero() {
    let runner = FakeRunner::with(|_| out(0, "garbage"));
    let host = HostInfo::probe(runner.as_ref(), "macos", "aarch64").await;
    assert_eq!(host.os, Os::MacOs { major: 0 });
}

#[tokio::test]
async fn other_platforms_map_to_their_variants() {
    let runner = FakeRunner::with(|_| out(1, ""));
    let win = HostInfo::probe(runner.as_ref(), "windows", "x86").await;
    assert_eq!((win.os, win.arch), (Os::Windows, Arch::Other));
    let bsd = HostInfo::probe(runner.as_ref(), "freebsd", "aarch64").await;
    assert_eq!(bsd.os, Os::Other);
}

#[tokio::test]
async fn current_probes_this_host_without_failing() {
    let host = HostInfo::current().await;
    if cfg!(target_os = "linux") {
        assert_eq!(host.os, Os::Linux);
    }
}
