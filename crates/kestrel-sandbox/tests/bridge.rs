//! The gateway bridge (issue #8): exact docker argv for every step, stale bridge replacement,
//! tolerated errors, cleanup after a failing step, idempotent stop and input validation.
//!
//! Step names carried by `SandboxError::Bridge`: `remove-stale`, `create-network`, `run`,
//! `connect`, `inspect`.
//!
//! The bridge URL uses the bridge's IP on `kestrel-gw`, read by `inspect`: CI showed that Docker's
//! embedded DNS does not resolve the name from a gVisor sandbox (`getent hosts kestrel-gateway`
//! exited 2 under `--runtime=runsc`, CI run 44563862878).

#![allow(clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{FakeRunner, s};
use kestrel_sandbox::{
    BridgeSpec, CommandOutput, DEFAULT_BRIDGE_CONNECTIONS, DEFAULT_BRIDGE_IMAGE, GatewayBridge,
    MAX_BRIDGE_CONNECTIONS, SandboxError,
};

const IMAGE: &str =
    "alpine/socat:1.8.0.3@sha256:beb4a68d9e4fe6b0f21ea774a0fde6c31f580dde6368939ed70100c5385b015e";
/// The database the bridge belongs to, and the id and container name derived from it (#29).
const DB: &str = "/tmp/a/kestrel.db";
const ID: &str = "b29b95f5";
const NAME: &str = "kestrel-gateway-b29b95f5";

fn rm() -> Vec<String> {
    s(&["docker", "rm", "--force", NAME])
}

/// Lists the containers that carry both ownership labels for this database.
fn ps() -> Vec<String> {
    s(&[
        "docker",
        "ps",
        "-a",
        "--filter",
        "label=kestrel.managed=bridge",
        "--filter",
        &format!("label=kestrel.db={ID}"),
        "--format",
        "{{.Names}}",
    ])
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
        NAME,
        "--label",
        "kestrel.managed=bridge",
        "--label",
        &format!("kestrel.db={ID}"),
        "--network",
        "bridge",
        "--add-host",
        "host.docker.internal:host-gateway",
        "--restart",
        "no",
        "--cap-drop",
        "ALL",
        "--read-only",
        "--security-opt",
        "no-new-privileges",
        "--pids-limit",
        "88",
        "--memory",
        "112m",
        "--user",
        "65534:65534",
        IMAGE,
        "-T",
        "300",
        &format!("TCP-LISTEN:{port},fork,reuseaddr,max-children=80"),
        &format!("TCP:host.docker.internal:{port}"),
    ])
}

fn connect() -> Vec<String> {
    s(&["docker", "network", "connect", "kestrel-gw", NAME])
}

const IP: &str = "172.18.0.2";

fn inspect() -> Vec<String> {
    s(&[
        "docker",
        "inspect",
        "--format",
        "{{(index .NetworkSettings.Networks \"kestrel-gw\").IPAddress}}",
        NAME,
    ])
}

fn ip_out(text: &str) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: text.to_string(),
        stderr: String::new(),
    }
}

/// Every command succeeds, and `inspect` answers with [`IP`] and a newline.
fn happy() -> Arc<FakeRunner> {
    FakeRunner::with(|argv| {
        if argv == inspect().as_slice() {
            ip_out(&format!("{IP}\n"))
        } else {
            ok()
        }
    })
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
        } else if argv == inspect().as_slice() {
            ip_out(IP)
        } else {
            ok()
        }
    })
}

async fn start(runner: &Arc<FakeRunner>, port: u16) -> Result<GatewayBridge, SandboxError> {
    GatewayBridge::start(runner.clone(), port, BridgeSpec::default(), Path::new(DB)).await
}

#[tokio::test]
async fn start_runs_the_five_steps_in_order_with_exact_argv() {
    let runner = happy();
    start(&runner, 7070).await.expect("bridge starts");
    assert_eq!(
        runner.runs(),
        vec![ps(), network(), run(7070), connect(), inspect()]
    );
    assert!(runner.spawns().is_empty(), "no streaming process is needed");
}

#[tokio::test]
async fn the_port_is_used_in_both_socat_addresses() {
    let runner = happy();
    start(&runner, 18080).await.expect("bridge starts");
    assert_eq!(runner.runs()[2], run(18080));
}

#[tokio::test]
async fn the_container_publishes_no_ports_and_never_restarts() {
    let runner = happy();
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
    let runner = happy();
    let spec = BridgeSpec {
        image: "registry.local/socat:9".to_string(),
        ..BridgeSpec::default()
    };
    GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
        .await
        .expect("bridge starts");
    assert!(runner.runs()[2].contains(&"registry.local/socat:9".to_string()));
    assert!(!runner.runs()[2].contains(&IMAGE.to_string()));
}

#[tokio::test]
async fn url_is_the_bridge_ip_and_the_port() {
    let runner = happy();
    let bridge = start(&runner, 7070).await.expect("bridge starts");
    assert_eq!(bridge.url().as_str(), "http://172.18.0.2:7070/");
    let other = start(&runner, 65535).await.expect("bridge starts");
    assert_eq!(other.url().as_str(), "http://172.18.0.2:65535/");
}

#[tokio::test]
async fn a_stale_bridge_is_removed_first_and_a_missing_one_is_fine() {
    // Stale container exists: rm succeeds. No container: rm says so on stderr.
    for rm_result in [
        ok(),
        fail("Error response from daemon: No such container: kestrel-gateway-b29b95f5"),
    ] {
        let runner = FakeRunner::with(move |argv| {
            if argv == ps().as_slice() {
                ip_out(&format!("{NAME}\n"))
            } else if argv == rm().as_slice() {
                rm_result.clone()
            } else if argv == inspect().as_slice() {
                ip_out(IP)
            } else {
                ok()
            }
        });
        start(&runner, 7070).await.expect("bridge starts");
        assert_eq!(runner.runs()[0], ps());
        assert_eq!(runner.runs()[1], rm());
        assert_eq!(runner.runs().len(), 6);
    }
}

#[tokio::test]
async fn an_existing_network_is_fine() {
    let runner = failing(
        network(),
        "Error response from daemon: network with name kestrel-gw already exists",
    );
    start(&runner, 7070).await.expect("bridge starts");
    assert_eq!(
        runner.runs(),
        vec![ps(), network(), run(7070), connect(), inspect()]
    );
}

#[tokio::test]
async fn other_rm_errors_are_not_tolerated_and_nothing_else_runs() {
    let runner = FakeRunner::with(|argv| {
        if argv == ps().as_slice() {
            ip_out(&format!("{NAME}\n"))
        } else if argv == rm().as_slice() {
            fail("permission denied")
        } else {
            ok()
        }
    });
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
        vec![ps(), rm()],
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
    assert_eq!(runner.runs(), vec![ps(), network()]);
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
    assert_eq!(runner.runs(), vec![ps(), network(), run(7070), rm()]);
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
        vec![ps(), network(), run(7070), connect(), rm()]
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
        } else if argv == inspect().as_slice() {
            ip_out(IP)
        } else {
            ok()
        }
    });
    let err = start(&runner, 7070).await.expect_err("fails");
    assert!(err.to_string().contains("connect broke"), "{err}");
}

#[tokio::test]
async fn a_failing_inspect_removes_the_container_again() {
    let runner = failing(inspect(), "No such container");
    let err = start(&runner, 7070).await.expect_err("fails");
    assert!(
        matches!(
            err,
            SandboxError::Bridge {
                step: "inspect",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("inspect"), "{err}");
    assert_eq!(
        runner.runs(),
        vec![ps(), network(), run(7070), connect(), inspect(), rm()]
    );
}

#[tokio::test]
async fn an_answer_that_is_not_an_ipv4_address_removes_the_container_again() {
    for answer in [
        "",
        "\n",
        "kestrel-gateway",
        "172.18.0.2 extra",
        "::1",
        "999.1.1.1",
    ] {
        let runner = FakeRunner::with(move |argv| {
            if argv == inspect().as_slice() {
                ip_out(answer)
            } else {
                ok()
            }
        });
        let err = start(&runner, 7070).await.expect_err(answer);
        assert!(
            matches!(
                err,
                SandboxError::Bridge {
                    step: "inspect",
                    ..
                }
            ),
            "{answer:?}: {err:?}"
        );
        assert_eq!(runner.runs().last(), Some(&rm()), "{answer:?}");
    }
}

#[tokio::test]
async fn stop_removes_the_container_once_and_is_idempotent() {
    let runner = happy();
    let bridge = start(&runner, 7070).await.expect("bridge starts");
    bridge.stop().await;
    assert_eq!(runner.runs().last(), Some(&rm()));
    let after_first = runner.runs().len();
    assert_eq!(after_first, 6);
    bridge.stop().await;
    assert_eq!(runner.runs().len(), after_first, "second stop runs nothing");
}

#[tokio::test]
async fn stop_ignores_a_container_that_is_already_gone_or_a_failing_rm() {
    for stderr in [
        "Error: No such container: kestrel-gateway-b29b95f5",
        "daemon not running",
    ] {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let runner = FakeRunner::with(move |argv| {
            if argv == rm().as_slice()
                && calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
            {
                fail(stderr)
            } else if argv == inspect().as_slice() {
                ip_out(IP)
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
    let runner = happy();
    let err = start(&runner, 0).await.expect_err("port 0");
    assert!(matches!(err, SandboxError::InvalidBridgePort), "{err:?}");
    assert!(runner.all_calls().is_empty());
}

/// Issue #80: the connection limit is a setting. It is socat's `max-children`, and the container's
/// process and memory limits grow with it: one process per connection plus socat itself and
/// spare, and 1 MiB per connection on top of 32 MiB.
#[tokio::test]
async fn the_connection_limit_sets_socat_children_pids_and_memory() {
    let runner = happy();
    let spec = BridgeSpec {
        connections: 200,
        ..BridgeSpec::default()
    };
    GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
        .await
        .expect("bridge starts");
    let argv = &runner.runs()[2];
    let value = |flag: &str| {
        let at = argv.iter().position(|a| a == flag).expect(flag);
        argv[at + 1].clone()
    };
    assert_eq!(value("--pids-limit"), "208");
    assert_eq!(value("--memory"), "232m");
    assert!(
        argv.contains(&"TCP-LISTEN:7070,fork,reuseaddr,max-children=200".to_string()),
        "{argv:?}"
    );
}

#[tokio::test]
async fn the_default_connection_limit_is_80_and_the_most_is_8192() {
    assert_eq!(DEFAULT_BRIDGE_CONNECTIONS, 80);
    assert_eq!(BridgeSpec::default().connections, DEFAULT_BRIDGE_CONNECTIONS);
    assert_eq!(MAX_BRIDGE_CONNECTIONS, 8192);
    let runner = happy();
    let spec = BridgeSpec {
        connections: MAX_BRIDGE_CONNECTIONS,
        ..BridgeSpec::default()
    };
    GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
        .await
        .expect("the most is allowed");
    assert!(runner.runs()[2].contains(&"--pids-limit".to_string()));
}

#[tokio::test]
async fn a_connection_limit_of_0_or_above_8192_is_rejected_before_any_command() {
    for connections in [0, MAX_BRIDGE_CONNECTIONS + 1, u32::MAX] {
        let runner = happy();
        let spec = BridgeSpec {
            connections,
            ..BridgeSpec::default()
        };
        let err = GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
            .await
            .expect_err("out of range");
        assert!(
            matches!(err, SandboxError::InvalidBridgeConnections { connections: c } if c == connections),
            "{connections}: {err:?}"
        );
        assert!(err.to_string().contains("1 to 8192"), "{err}");
        assert!(runner.all_calls().is_empty(), "{connections}");
    }
}

#[tokio::test]
async fn bad_images_are_rejected_before_any_command() {
    for image in ["", "-v", "--privileged", "--rm=true"] {
        let runner = happy();
        let spec = BridgeSpec {
            image: image.to_string(),
            ..BridgeSpec::default()
        };
        let err = GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
            .await
            .expect_err("bad image");
        assert!(
            matches!(err, SandboxError::InvalidImage { .. }),
            "{image:?}: {err:?}"
        );
        assert!(runner.all_calls().is_empty(), "{image:?}");
    }
}
