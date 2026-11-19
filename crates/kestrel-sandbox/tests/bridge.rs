//! The gateway bridge (issue #8): exact docker argv for every step, stale bridge replacement,
//! tolerated errors, cleanup after a failing step, idempotent stop and input validation.
//!
//! Step names carried by `SandboxError::Bridge`: `remove-stale`, `create-network`, `run`,
//! `connect`.

#![allow(clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{FakeRunner, s};
use kestrel_sandbox::{
    BridgeSpec, CommandOutput, DEFAULT_BRIDGE_IMAGE, GatewayBridge, SandboxError,
};

const IMAGE: &str = "alpine/socat:1.8.0.3";

fn rm() -> Vec<String> {
    s(&["docker", "rm", "--force", "kestrel-gateway"])
}

fn network() -> Vec<String> {
    s(&["docker", "network", "create", "--internal", "kestrel-gw"])
}

fn run(port: u16) -> Vec<String> {
    s(&[
        "docker",
        "run",
        "--detach",
        "--name",
        "kestrel-gateway",
        "--network",
        "bridge",
        "--add-host",
        "host.docker.internal:host-gateway",
        "--restart",
        "no",
        IMAGE,
        &format!("TCP-LISTEN:{port},fork,reuseaddr"),
        &format!("TCP:host.docker.internal:{port}"),
    ])
}

fn connect() -> Vec<String> {
    s(&[
        "docker",
        "network",
        "connect",
        "kestrel-gw",
        "kestrel-gateway",
    ])
}

fn fail(stderr: &str) -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

fn ok() -> CommandOutput {
    CommandOutput::default()
}

/// A runner that answers `fail_with` to the argv `bad` and succeeds otherwise.
fn failing(bad: Vec<String>, stderr: &'static str) -> Arc<FakeRunner> {
    FakeRunner::with(move |argv| {
        if argv == bad.as_slice() {
            fail(stderr)
        } else {
            ok()
        }
    })
}

async fn start(runner: &Arc<FakeRunner>, port: u16) -> Result<GatewayBridge, SandboxError> {
    GatewayBridge::start(runner.clone(), port, BridgeSpec::default()).await
}

#[tokio::test]
async fn start_runs_the_four_steps_in_order_with_exact_argv() {
    let runner = FakeRunner::with(|_| ok());
    start(&runner, 7070).await.expect("bridge starts");
    assert_eq!(runner.runs(), vec![rm(), network(), run(7070), connect()]);
    assert!(runner.spawns().is_empty(), "no streaming process is needed");
}

#[tokio::test]
async fn the_port_is_used_in_both_socat_addresses() {
    let runner = FakeRunner::with(|_| ok());
    start(&runner, 18080).await.expect("bridge starts");
    assert_eq!(runner.runs()[2], run(18080));
}

#[tokio::test]
async fn the_container_publishes_no_ports_and_never_restarts() {
    let runner = FakeRunner::with(|_| ok());
    start(&runner, 7070).await.expect("bridge starts");
    let argv = &runner.runs()[2];
    for flag in ["-p", "--publish", "-P", "--publish-all", "--privileged"] {
        assert!(!argv.iter().any(|a| a == flag), "{flag} in {argv:?}");
    }
    let at = argv
        .iter()
        .position(|a| a == "--restart")
        .expect("--restart");
    assert_eq!(argv[at + 1], "no");
}

#[tokio::test]
async fn the_default_image_is_pinned() {
    assert_eq!(DEFAULT_BRIDGE_IMAGE, IMAGE);
    assert_eq!(BridgeSpec::default().image, IMAGE);
    assert!(!BridgeSpec::default().image.ends_with(":latest"));
    assert!(BridgeSpec::default().image.contains(':'));
}

#[tokio::test]
async fn a_custom_image_is_used_as_is() {
    let runner = FakeRunner::with(|_| ok());
    let spec = BridgeSpec {
        image: "registry.local/socat:9".to_string(),
    };
    GatewayBridge::start(runner.clone(), 7070, spec)
        .await
        .expect("bridge starts");
    assert!(runner.runs()[2].contains(&"registry.local/socat:9".to_string()));
    assert!(!runner.runs()[2].contains(&IMAGE.to_string()));
}

#[tokio::test]
async fn url_is_the_bridge_name_and_the_port() {
    let runner = FakeRunner::with(|_| ok());
    let bridge = start(&runner, 7070).await.expect("bridge starts");
    assert_eq!(bridge.url().as_str(), "http://kestrel-gateway:7070/");
    let other = start(&runner, 65535).await.expect("bridge starts");
    assert_eq!(other.url().as_str(), "http://kestrel-gateway:65535/");
}

#[tokio::test]
async fn a_stale_bridge_is_removed_first_and_a_missing_one_is_fine() {
    // Stale container exists: rm succeeds. No container: rm says so on stderr.
    for rm_result in [
        ok(),
        fail("Error response from daemon: No such container: kestrel-gateway"),
    ] {
        let runner = FakeRunner::with(move |argv| {
            if argv == rm().as_slice() {
                rm_result.clone()
            } else {
                ok()
            }
        });
        start(&runner, 7070).await.expect("bridge starts");
        assert_eq!(runner.runs()[0], rm());
        assert_eq!(runner.runs().len(), 4);
    }
}

#[tokio::test]
async fn an_existing_network_is_fine() {
    let runner = failing(
        network(),
        "Error response from daemon: network with name kestrel-gw already exists",
    );
    start(&runner, 7070).await.expect("bridge starts");
    assert_eq!(runner.runs(), vec![rm(), network(), run(7070), connect()]);
}

#[tokio::test]
async fn other_rm_errors_are_not_tolerated_and_nothing_else_runs() {
    let runner = failing(rm(), "permission denied");
    let err = start(&runner, 7070).await.expect_err("fails");
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
    assert!(err.to_string().contains("permission denied"), "{err}");
    assert_eq!(
        runner.runs(),
        vec![rm()],
        "no container exists yet, no extra cleanup"
    );
}

#[tokio::test]
async fn a_failing_network_create_names_the_step_and_creates_nothing() {
    let runner = failing(network(), "daemon not running");
    let err = start(&runner, 7070).await.expect_err("fails");
    assert!(
        matches!(
            err,
            SandboxError::Bridge {
                step: "create-network",
                ..
            }
        ),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("create-network") && text.contains("daemon not running"),
        "{text}"
    );
    assert_eq!(runner.runs(), vec![rm(), network()]);
}

#[tokio::test]
async fn a_failing_run_removes_the_container_again() {
    let runner = failing(run(7070), "pull access denied");
    let err = start(&runner, 7070).await.expect_err("fails");
    assert!(
        matches!(err, SandboxError::Bridge { step: "run", .. }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("run") && text.contains("pull access denied"),
        "{text}"
    );
    assert_eq!(runner.runs(), vec![rm(), network(), run(7070), rm()]);
}

#[tokio::test]
async fn a_failing_connect_removes_the_container_again() {
    let runner = failing(connect(), "network kestrel-gw not found");
    let err = start(&runner, 7070).await.expect_err("fails");
    assert!(
        matches!(
            err,
            SandboxError::Bridge {
                step: "connect",
                ..
            }
        ),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("connect") && text.contains("network kestrel-gw not found"),
        "{text}"
    );
    assert_eq!(
        runner.runs(),
        vec![rm(), network(), run(7070), connect(), rm()]
    );
}

#[tokio::test]
async fn cleanup_failing_too_still_reports_the_original_error() {
    // Every rm after the first fails; the error must still be the connect failure.
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let runner = FakeRunner::with(move |argv| {
        if argv == connect().as_slice() {
            fail("connect broke")
        } else if argv == rm().as_slice()
            && calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
        {
            fail("rm broke")
        } else {
            ok()
        }
    });
    let err = start(&runner, 7070).await.expect_err("fails");
    assert!(err.to_string().contains("connect broke"), "{err}");
}

#[tokio::test]
async fn stop_removes_the_container_once_and_is_idempotent() {
    let runner = FakeRunner::with(|_| ok());
    let bridge = start(&runner, 7070).await.expect("bridge starts");
    bridge.stop().await;
    assert_eq!(runner.runs().last(), Some(&rm()));
    let after_first = runner.runs().len();
    assert_eq!(after_first, 5);
    bridge.stop().await;
    assert_eq!(runner.runs().len(), after_first, "second stop runs nothing");
}

#[tokio::test]
async fn stop_ignores_a_container_that_is_already_gone_or_a_failing_rm() {
    for stderr in [
        "Error: No such container: kestrel-gateway",
        "daemon not running",
    ] {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let runner = FakeRunner::with(move |argv| {
            if argv == rm().as_slice()
                && calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
            {
                fail(stderr)
            } else {
                ok()
            }
        });
        let bridge = start(&runner, 7070).await.expect("bridge starts");
        bridge.stop().await; // must not panic
        bridge.stop().await;
    }
}

#[tokio::test]
async fn port_zero_is_rejected_before_any_command() {
    let runner = FakeRunner::with(|_| ok());
    let err = start(&runner, 0).await.expect_err("port 0");
    assert!(matches!(err, SandboxError::InvalidBridgePort), "{err:?}");
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn bad_images_are_rejected_before_any_command() {
    for image in ["", "-v", "--privileged", "--rm=true"] {
        let runner = FakeRunner::with(|_| ok());
        let spec = BridgeSpec {
            image: image.to_string(),
        };
        let err = GatewayBridge::start(runner.clone(), 7070, spec)
            .await
            .expect_err("bad image");
        assert!(
            matches!(err, SandboxError::InvalidImage { .. }),
            "{image:?}: {err:?}"
        );
        assert!(runner.all_calls().is_empty(), "{image:?}");
    }
}
