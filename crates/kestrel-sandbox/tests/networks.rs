//! Issue #10: one internal network per run. With a bridge set, `prepare_network` creates the
//! run's network, joins the shared gateway bridge to it and reads the bridge's IP there. `create`
//! attaches the sandbox to that network only, and `destroy` disconnects the bridge and removes
//! the network again.
//!
//! Known limit, not solved here (issue #34, milestone 2): Docker's default address pools hold
//! about 30 networks. Pool exhaustion becomes an error that names the limit and #34.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::net::IpAddr;
use std::sync::Arc;

use common::{FakeRunner, ID, s, spec};
use kestrel_sandbox::{
    AppleContainerDriver, CommandOutput, DockerRuncDriver, GvisorDriver, INSTANCE_LABEL,
    NetworkPolicy, PreparedNetwork, RUN_LABEL, SandboxDriver, SandboxError, SandboxHandle,
    shared_network_warning,
};
use kestrel_types::RunId;
use url::Url;

const RUN: &str = "0198c5a2-7b1e-7c3d-9f00-1a2b3c4d5e6f";
const NET: &str = "kestrel-gw-1a2b3c4d5e6f";
const RUN2: &str = "0198c5a2-7b1e-7c3d-9f00-aaaaaaaaaaaa";
const NET2: &str = "kestrel-gw-aaaaaaaaaaaa";
const BRIDGE: &str = "kestrel-bridge";

fn run(id: &str) -> RunId {
    id.parse().expect("run id")
}

fn inspect_argv(net: &str) -> Vec<String> {
    s(&[
        "docker",
        "inspect",
        "--format",
        &format!("{{{{(index .NetworkSettings.Networks \"{net}\").IPAddress}}}}"),
        BRIDGE,
    ])
}

fn create_net(net: &str, run: &str) -> Vec<String> {
    s(&[
        "docker",
        "network",
        "create",
        "--internal",
        "--label",
        &format!("kestrel.run={run}"),
        net,
    ])
}

fn connect(net: &str) -> Vec<String> {
    s(&["docker", "network", "connect", net, BRIDGE])
}

fn disconnect(net: &str) -> Vec<String> {
    s(&["docker", "network", "disconnect", net, BRIDGE])
}

fn rm_net(net: &str) -> Vec<String> {
    s(&["docker", "network", "rm", net])
}

fn rm_container() -> Vec<String> {
    s(&["docker", "rm", "--force", ID])
}

fn ok(stdout: &str) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

fn fail(stderr: &str) -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: format!("{stderr}\n"),
    }
}

fn is(argv: &[String], a: &str, b: &str) -> bool {
    argv.get(1).map(String::as_str) == Some(a) && argv.get(2).map(String::as_str) == Some(b)
}

/// Every call succeeds. `inspect` answers with `ip`; `docker create` prints the container id.
fn answering(ip: &'static str) -> Arc<FakeRunner> {
    FakeRunner::with(move |argv| {
        if argv.get(1).map(String::as_str) == Some("inspect") {
            ok(&format!("{ip}\n"))
        } else {
            ok(&format!("{ID}\n"))
        }
    })
}

/// Like [`answering`], but the call matching `a b` fails with `stderr`.
fn failing_at(a: &'static str, b: &'static str, stderr: &'static str) -> Arc<FakeRunner> {
    FakeRunner::with(move |argv| {
        if is(argv, a, b) || (a == "inspect" && argv.get(1).map(String::as_str) == Some("inspect"))
        {
            fail(stderr)
        } else {
            ok(&format!("{ID}\n"))
        }
    })
}

fn gvisor(runner: &Arc<FakeRunner>) -> GvisorDriver {
    GvisorDriver::new(runner.clone()).with_bridge(BRIDGE)
}

fn gateway_spec(network: Option<&str>) -> kestrel_sandbox::LaunchSpec {
    let mut spec = spec();
    spec.network = NetworkPolicy::GatewayOnly {
        gateway: Url::parse("http://172.20.0.2:7070/r/tok").unwrap(),
    };
    spec.prepared_network = network.map(String::from);
    spec
}

/// The argv of the sandbox `create` (not `network create`).
fn sandbox_create(runner: &FakeRunner) -> Vec<String> {
    runner
        .runs()
        .into_iter()
        .find(|a| a.get(1).map(String::as_str) == Some("create") && !a.contains(&"network".into()))
        .expect("a sandbox create call")
}

// 1. Argv sequence ------------------------------------------------------------------------------

#[tokio::test]
async fn s1_prepare_runs_create_connect_inspect_in_that_order() {
    let runner = answering("172.20.0.2");
    let prepared = gvisor(&runner)
        .prepare_network(run(RUN))
        .await
        .expect("prepare")
        .expect("a bridge is set, so a network is prepared");
    assert_eq!(
        prepared,
        PreparedNetwork {
            network: NET.into(),
            gateway_host: "172.20.0.2".parse::<IpAddr>().unwrap(),
        }
    );
    assert_eq!(
        runner.all_calls(),
        vec![create_net(NET, RUN), connect(NET), inspect_argv(NET)]
    );
}

#[tokio::test]
async fn s1_the_instance_label_is_added_when_the_driver_has_an_instance() {
    let runner = answering("172.20.0.2");
    gvisor(&runner)
        .with_instance("1a2b3c4d")
        .prepare_network(run(RUN))
        .await
        .expect("prepare");
    let mut want = create_net(NET, RUN);
    want.splice(
        6..6,
        s(&["--label", "kestrel.instance=1a2b3c4d"]).into_iter(),
    );
    assert_eq!(runner.runs()[0], want);
    assert_eq!(RUN_LABEL, "kestrel.run");
    assert_eq!(INSTANCE_LABEL, "kestrel.instance");
}

#[tokio::test]
async fn s1_docker_runc_prepares_the_same_way() {
    let runner = answering("172.20.0.2");
    let prepared = DockerRuncDriver::new(runner.clone())
        .with_bridge(BRIDGE)
        .prepare_network(run(RUN))
        .await
        .expect("prepare");
    assert!(prepared.is_some());
    assert_eq!(runner.runs()[0], create_net(NET, RUN));
}

#[tokio::test]
async fn s1_create_attaches_the_prepared_network_only() {
    let runner = answering("172.20.0.2");
    gvisor(&runner)
        .create(&gateway_spec(Some(NET)))
        .await
        .expect("create");
    // The old shared `kestrel-gw` creation is skipped: the only call is the sandbox create.
    assert_eq!(runner.runs().len(), 1, "{:?}", runner.runs());
    let argv = sandbox_create(&runner);
    let nets: Vec<&String> = argv
        .windows(2)
        .filter(|w| w[0] == "--network")
        .map(|w| &w[1])
        .collect();
    assert_eq!(nets, vec![NET]);
    assert!(!argv.iter().any(|a| a == "kestrel-gw"), "{argv:?}");
}

#[tokio::test]
async fn s1_without_a_prepared_network_create_is_exactly_as_before() {
    let runner = answering("172.20.0.2");
    gvisor(&runner)
        .create(&gateway_spec(None))
        .await
        .expect("create");
    assert_eq!(
        runner.runs()[0],
        s(&["docker", "network", "create", "--internal", "kestrel-gw"])
    );
    let argv = sandbox_create(&runner);
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--network" && w[1] == "kestrel-gw"),
        "{argv:?}"
    );
}

#[tokio::test]
async fn s1_without_a_bridge_prepare_is_none_and_runs_no_process() {
    let runner = answering("172.20.0.2");
    let got = GvisorDriver::new(runner.clone())
        .prepare_network(run(RUN))
        .await
        .expect("prepare");
    assert_eq!(got, None);
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn s1_apple_container_never_prepares_a_network() {
    let runner = answering("172.20.0.2");
    let got = AppleContainerDriver::new(runner.clone())
        .with_bridge(BRIDGE)
        .prepare_network(run(RUN))
        .await
        .expect("prepare");
    assert_eq!(got, None);
    assert!(runner.all_calls().is_empty());
}

// 2. Two runs -------------------------------------------------------------------------------------

#[tokio::test]
async fn s2_two_runs_get_different_networks_and_their_own_bridge_ip() {
    let runner = FakeRunner::with(|argv| {
        if argv.get(1).map(String::as_str) == Some("inspect") {
            let text = argv.join(" ");
            ok(if text.contains(NET2) {
                "172.21.0.2\n"
            } else {
                "172.20.0.2\n"
            })
        } else {
            ok(&format!("{ID}\n"))
        }
    });
    let driver = gvisor(&runner);
    let a = driver
        .prepare_network(run(RUN))
        .await
        .unwrap()
        .expect("net a");
    let b = driver
        .prepare_network(run(RUN2))
        .await
        .unwrap()
        .expect("net b");
    assert_eq!((a.network.as_str(), b.network.as_str()), (NET, NET2));
    assert_eq!(a.gateway_host.to_string(), "172.20.0.2");
    assert_eq!(b.gateway_host.to_string(), "172.21.0.2");
}

// 3. Destroy --------------------------------------------------------------------------------------

/// Creates a sandbox on `NET` and returns its handle.
async fn running_on_net(driver: &GvisorDriver) -> SandboxHandle {
    driver
        .create(&gateway_spec(Some(NET)))
        .await
        .expect("create")
}

#[tokio::test]
async fn s3_destroy_removes_the_container_then_disconnects_and_removes_the_network() {
    let runner = answering("172.20.0.2");
    let driver = gvisor(&runner);
    let h = running_on_net(&driver).await;
    let before = runner.runs().len();
    driver.destroy(&h).await.expect("destroy");
    assert_eq!(
        runner.runs()[before..].to_vec(),
        vec![rm_container(), disconnect(NET), rm_net(NET)]
    );
}

#[tokio::test]
async fn s3_a_second_destroy_is_a_no_op_for_the_network() {
    let runner = answering("172.20.0.2");
    let driver = gvisor(&runner);
    let h = running_on_net(&driver).await;
    driver.destroy(&h).await.expect("first");
    assert!(
        runner.runs().contains(&rm_net(NET)),
        "first destroy left the network"
    );
    let before = runner.runs().len();
    driver.destroy(&h).await.expect("second");
    assert_eq!(runner.runs()[before..].to_vec(), vec![rm_container()]);
}

#[tokio::test]
async fn s3_not_found_errors_are_tolerated() {
    for text in [
        "Error response from daemon: No such network: kestrel-gw-1a2b3c4d5e6f",
        "Error response from daemon: container kestrel-bridge is not connected to network kestrel-gw-1a2b3c4d5e6f",
        "Error: network kestrel-gw-1a2b3c4d5e6f not found",
    ] {
        let runner = FakeRunner::with(move |argv| {
            if argv.get(1).map(String::as_str) == Some("network")
                && matches!(argv.get(2).map(String::as_str), Some("disconnect" | "rm"))
            {
                fail(text)
            } else {
                ok(&format!("{ID}\n"))
            }
        });
        let driver = gvisor(&runner);
        let h = running_on_net(&driver).await;
        driver
            .destroy(&h)
            .await
            .unwrap_or_else(|e| panic!("{text}: {e}"));
        assert!(
            runner.runs().contains(&rm_net(NET)),
            "{text}: network not removed"
        );
    }
}

#[tokio::test]
async fn s3_a_real_network_error_is_reported_and_a_retry_tries_again() {
    let runner = FakeRunner::with(|argv| {
        if is(argv, "network", "disconnect") {
            fail("Error response from daemon: boom")
        } else {
            ok(&format!("{ID}\n"))
        }
    });
    let driver = gvisor(&runner);
    let h = running_on_net(&driver).await;
    let err = driver.destroy(&h).await.expect_err("disconnect fails");
    assert!(err.to_string().contains("boom"), "{err}");
    let before = runner.runs().len();
    let _ = driver.destroy(&h).await;
    assert!(
        runner.runs()[before..].contains(&disconnect(NET)),
        "the retry did not try the network again"
    );
}

#[tokio::test]
async fn s3_destroy_of_a_sandbox_without_a_prepared_network_is_unchanged() {
    let runner = answering("172.20.0.2");
    let driver = gvisor(&runner);
    let h = driver.create(&gateway_spec(None)).await.expect("create");
    let before = runner.runs().len();
    driver.destroy(&h).await.expect("destroy");
    assert_eq!(runner.runs()[before..].to_vec(), vec![rm_container()]);
}

#[tokio::test]
async fn s3_remove_network_disconnects_then_removes_and_is_idempotent() {
    let runner = FakeRunner::with(|argv| {
        if is(argv, "network", "disconnect") || is(argv, "network", "rm") {
            fail("Error: No such network: x")
        } else {
            ok("")
        }
    });
    let driver = gvisor(&runner);
    driver.remove_network(NET).await.expect("tolerated");
    driver.remove_network(NET).await.expect("again");
    assert_eq!(
        runner.runs(),
        vec![disconnect(NET), rm_net(NET), disconnect(NET), rm_net(NET)]
    );
}

#[tokio::test]
async fn s3_remove_network_without_a_bridge_only_removes() {
    let runner = answering("");
    GvisorDriver::new(runner.clone())
        .remove_network(NET)
        .await
        .expect("remove");
    assert_eq!(runner.runs(), vec![rm_net(NET)]);
}

#[tokio::test]
async fn s3_an_unsafe_network_name_is_refused_and_runs_no_process() {
    for bad in ["", "-x", "a b", "x;rm", "$(id)"] {
        let runner = answering("");
        let err = gvisor(&runner)
            .remove_network(bad)
            .await
            .expect_err("invalid name");
        assert!(matches!(err, SandboxError::InvalidId { .. }), "{bad:?}");
        assert!(runner.all_calls().is_empty(), "{bad:?}");
    }
}

// 4. Failures clean up ----------------------------------------------------------------------------

#[tokio::test]
async fn s4_network_create_fails_nothing_to_clean_up_and_the_step_is_named() {
    let runner = failing_at("network", "create", "Error response from daemon: boom");
    let err = gvisor(&runner)
        .prepare_network(run(RUN))
        .await
        .expect_err("fails");
    assert!(err.to_string().contains("network create"), "{err}");
    assert_eq!(runner.runs(), vec![create_net(NET, RUN)]);
}

#[tokio::test]
async fn s4_connect_fails_the_network_is_removed() {
    let runner = failing_at("network", "connect", "Error response from daemon: boom");
    let err = gvisor(&runner)
        .prepare_network(run(RUN))
        .await
        .expect_err("fails");
    assert!(err.to_string().contains("network connect"), "{err}");
    assert!(err.to_string().contains("boom"), "{err}");
    assert_eq!(
        runner.runs(),
        vec![create_net(NET, RUN), connect(NET), rm_net(NET)]
    );
}

#[tokio::test]
async fn s4_inspect_fails_the_bridge_is_disconnected_and_the_network_removed() {
    let runner = failing_at("inspect", "", "Error: boom");
    let err = gvisor(&runner)
        .prepare_network(run(RUN))
        .await
        .expect_err("fails");
    assert!(err.to_string().contains("inspect"), "{err}");
    assert_eq!(
        runner.runs(),
        vec![
            create_net(NET, RUN),
            connect(NET),
            inspect_argv(NET),
            disconnect(NET),
            rm_net(NET)
        ]
    );
}

#[tokio::test]
async fn s4_a_failing_cleanup_does_not_hide_the_original_error() {
    let runner = FakeRunner::with(|argv| {
        if is(argv, "network", "connect") {
            fail("Error: connect boom")
        } else if is(argv, "network", "rm") {
            fail("Error: rm boom")
        } else {
            ok("")
        }
    });
    let err = gvisor(&runner)
        .prepare_network(run(RUN))
        .await
        .expect_err("fails");
    assert!(err.to_string().contains("connect boom"), "{err}");
}

// IP validation -----------------------------------------------------------------------------------

#[tokio::test]
async fn bad_inspect_answers_are_errors_that_clean_up() {
    for answer in [
        "",
        "\n",
        "not an ip",
        "172.20.0.2 extra",
        "::1",
        "<no value>",
        "999.1.1.1",
    ] {
        let runner = answering(Box::leak(answer.to_string().into_boxed_str()));
        let err = gvisor(&runner)
            .prepare_network(run(RUN))
            .await
            .expect_err(answer);
        assert!(err.to_string().contains("inspect"), "{answer:?}: {err}");
        let runs = runner.runs();
        assert_eq!(
            runs[runs.len() - 2..],
            [disconnect(NET), rm_net(NET)],
            "{answer:?}"
        );
    }
}

#[tokio::test]
async fn an_answer_with_surrounding_whitespace_is_accepted() {
    let runner = answering("  172.20.0.2  ");
    let got = gvisor(&runner)
        .prepare_network(run(RUN))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.gateway_host.to_string(), "172.20.0.2");
}

// 5. Pool exhaustion ------------------------------------------------------------------------------

#[tokio::test]
async fn s5_pool_exhaustion_names_the_limit_and_issue_34() {
    for text in [
        "Error response from daemon: all predefined address pools have been fully subnetted",
        "Error response from daemon: Pool overlaps with other one on this address space",
    ] {
        let runner = failing_at("network", "create", text);
        let err = gvisor(&runner)
            .prepare_network(run(RUN))
            .await
            .expect_err("fails");
        assert!(
            matches!(err, SandboxError::NetworkPoolExhausted { .. }),
            "{err:?}"
        );
        let shown = err.to_string();
        assert!(shown.contains("#34"), "{shown}");
        assert!(shown.contains("30"), "{shown}");
        assert!(
            !shown.contains('\u{2014}') && !shown.contains('\u{2013}'),
            "{shown}"
        );
        assert_eq!(
            runner.runs().len(),
            1,
            "nothing was created, nothing to remove"
        );
    }
}

// 6. Leftover networks ----------------------------------------------------------------------------

fn ls_argv(instance: Option<&str>) -> Vec<String> {
    let mut argv = s(&["docker", "network", "ls", "--filter", "label=kestrel.run"]);
    if let Some(id) = instance {
        argv.extend(s(&["--filter", &format!("label=kestrel.instance={id}")]));
    }
    argv.extend(s(&["--format", "{{.Name}}"]));
    argv
}

#[tokio::test]
async fn s6_leftover_networks_filters_by_run_label_and_instance() {
    let runner = answering("");
    let _ = runner;
    let runner = FakeRunner::with(|_| {
        ok("kestrel-gw-1a2b3c4d5e6f\n\nkestrel-gw-aaaaaaaaaaaa\nbad name\n-x\n")
    });
    let found = gvisor(&runner)
        .with_instance("1a2b3c4d")
        .leftover_networks()
        .await
        .expect("leftover networks");
    assert_eq!(found, vec![NET.to_string(), NET2.to_string()]);
    assert_eq!(runner.all_calls(), vec![ls_argv(Some("1a2b3c4d"))]);
}

#[tokio::test]
async fn s6_without_an_instance_there_is_no_instance_filter() {
    let runner = FakeRunner::with(|_| ok(""));
    gvisor(&runner).leftover_networks().await.expect("ok");
    assert_eq!(runner.all_calls(), vec![ls_argv(None)]);
}

#[tokio::test]
async fn s6_an_unsafe_instance_id_is_refused_and_runs_no_process() {
    let runner = FakeRunner::with(|_| ok(""));
    let err = gvisor(&runner)
        .with_instance("a b")
        .leftover_networks()
        .await
        .expect_err("invalid");
    assert!(matches!(err, SandboxError::InvalidId { .. }));
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn s6_apple_has_no_leftover_networks_and_runs_no_process() {
    let runner = answering("");
    let found = AppleContainerDriver::new(runner.clone())
        .leftover_networks()
        .await
        .expect("ok");
    assert!(found.is_empty());
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn s6_a_failing_listing_is_an_error() {
    let runner = FakeRunner::with(|_| fail("Cannot connect to the Docker daemon"));
    let err = gvisor(&runner)
        .leftover_networks()
        .await
        .expect_err("fails");
    assert!(err.to_string().contains("Docker daemon"));
}

// 7. No bridge ------------------------------------------------------------------------------------

#[test]
fn s7_with_a_bridge_there_is_no_warning() {
    assert_eq!(shared_network_warning(true), None);
}

#[test]
fn s7_without_a_bridge_there_is_one_plain_warning_line() {
    let line = shared_network_warning(false).expect("a warning");
    assert!(!line.contains('\n'), "{line:?}");
    assert!(line.contains("reach each other"), "{line}");
    assert!(
        !line.contains('\u{2014}') && !line.contains('\u{2013}'),
        "{line}"
    );
}

// Detection ---------------------------------------------------------------------------------------

#[test]
fn detect_with_bridge_builds_the_same_drivers_as_detect_for() {
    use kestrel_sandbox::{Arch, HostInfo, Os, detect_for, detect_with_bridge};
    let host = HostInfo {
        os: Os::Linux,
        arch: Arch::X86_64,
        docker_runtimes: vec!["runc".into(), "runsc".into()],
        apple_container: false,
    };
    let names =
        |drivers: Vec<Box<dyn SandboxDriver>>| drivers.iter().map(|d| d.name()).collect::<Vec<_>>();
    assert_eq!(
        names(detect_with_bridge(&host, true, Some("1a2b3c4d"), Some(BRIDGE)).unwrap()),
        names(detect_for(&host, true, Some("1a2b3c4d")).unwrap())
    );
}
