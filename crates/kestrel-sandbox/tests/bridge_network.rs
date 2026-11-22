//! Issue #99: the bridge's own `kestrel-gw` network gets an explicit subnet from Kestrel's plan,
//! so serve starts on a host whose docker default address pools are used up.
//!
//! * `SubnetPool::claim_span` takes an aligned span out of the plan for good: a `/24` (32 blocks)
//!   by default, so many serve instances' bridges fit on one daemon, and smaller only when the
//!   pool cannot hold it plus one run block. No run network ever gets a block of it.
//! * `GatewayBridge::plan_network` reuses an existing `kestrel-gw`, recreates one that is smaller
//!   than the span when nothing is attached to it, and otherwise claims a span.
//! * `GatewayBridge::start` creates `kestrel-gw` with `--subnet` and `--gateway` from the span,
//!   never runs `network create` for an existing one, and says clearly when `kestrel-gw` is full.

#![allow(clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{FakeRunner, s};
use kestrel_sandbox::{
    BRIDGE_NETWORK_PREFIX, BridgeNetwork, BridgeSpec, CommandOutput, GatewayBridge, Ipv4Cidr,
    NetworkPlan, SandboxError, SubnetPool,
};

const DB: &str = "/srv/kestrel/kestrel.db";
const IP: &str = "10.200.0.2";

fn cidr(text: &str) -> Ipv4Cidr {
    Ipv4Cidr::parse(text).expect("valid cidr")
}

fn pool(text: &str) -> SubnetPool {
    SubnetPool::parse(text).expect("valid pool")
}

fn name() -> String {
    GatewayBridge::container_name(&GatewayBridge::instance_id(Path::new(DB)))
}

/// How the bridge looks for an existing `kestrel-gw`.
fn lookup() -> Vec<String> {
    s(&[
        "docker",
        "network",
        "ls",
        "--filter",
        "name=^kestrel-gw$",
        "--format",
        "{{.Name}}",
    ])
}

/// How the bridge reads an existing `kestrel-gw`: attached containers, then its subnets.
fn inspect_gw() -> Vec<String> {
    s(&[
        "docker",
        "network",
        "inspect",
        "--format",
        "{{len .Containers}};{{range .IPAM.Config}}{{.Subnet}} {{end}}",
        "kestrel-gw",
    ])
}

fn rm_gw() -> Vec<String> {
    s(&["docker", "network", "rm", "kestrel-gw"])
}

fn out(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
    CommandOutput {
        status,
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
    }
}

/// Docker that answers `lookup` with `listed`, `inspect_gw` with `gw`, the bridge's address on
/// `kestrel-gw` with [`IP`], and succeeds otherwise.
fn docker_with(listed: &'static str, gw: &'static str) -> Arc<FakeRunner> {
    FakeRunner::with(move |argv| {
        if argv == lookup().as_slice() {
            out(0, listed, "")
        } else if argv == inspect_gw().as_slice() {
            out(0, gw, "")
        } else if argv.get(1).map(String::as_str) == Some("inspect") {
            out(0, &format!("{IP}\n"), "")
        } else {
            out(0, "", "")
        }
    })
}

/// Docker with no `kestrel-gw` (`""`) or one that cannot be read.
fn docker(listed: &'static str) -> Arc<FakeRunner> {
    docker_with(listed, "")
}

fn creates(runner: &FakeRunner) -> Vec<Vec<String>> {
    runner
        .runs()
        .into_iter()
        .filter(|a| a.get(1).map(String::as_str) == Some("network") && a[2] == "create")
        .collect()
}

async fn plan(runner: &FakeRunner, plan: &SubnetPool) -> NetworkPlan {
    GatewayBridge::plan_network(runner, plan)
        .await
        .expect("planned")
}

async fn start(runner: &Arc<FakeRunner>, network: BridgeNetwork) -> GatewayBridge {
    let spec = BridgeSpec {
        network,
        ..BridgeSpec::default()
    };
    GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
        .await
        .expect("bridge starts")
}

fn span_of(network: &BridgeNetwork) -> Ipv4Cidr {
    match network {
        BridgeNetwork::Subnet(span) => span.cidr(),
        other => panic!("expected a span for kestrel-gw, got {other:?}"),
    }
}

// SubnetPool::claim_span --------------------------------------------------------------------------

#[test]
fn the_bridge_span_is_a_24_by_default() {
    assert_eq!(BRIDGE_NETWORK_PREFIX, 24);
}

#[test]
fn a_span_of_the_size_asked_for_is_claimed_aligned_and_no_run_block_overlaps_it() {
    let plan = pool("10.200.0.0/16");
    plan.exclude(&[cidr("10.200.0.8/29")]);
    let held = plan.reserve().expect("a run block");
    assert_eq!(held.cidr(), cidr("10.200.0.0/29"));
    // The first /24 has a taken block and a block in use, so the span is the next aligned one.
    let span = plan.claim_span(24).expect("a span");
    assert_eq!(span.cidr(), cidr("10.200.1.0/24"));
    assert_eq!(span.gateway().to_string(), "10.200.1.1");
    assert_eq!(plan.available(), 8192 - 2 - 32);
    assert_eq!(plan.in_use(), 1, "the span is not a run's block");

    // Releasing the span, as a run would, changes nothing.
    plan.release(&span);
    assert_eq!(plan.available(), 8192 - 2 - 32);
    plan.release(&held);
    let mut runs = 0;
    while let Some(run) = plan.reserve() {
        assert!(!run.cidr().overlaps(&span.cidr()), "{:?}", run.cidr());
        runs += 1;
    }
    assert_eq!(runs, 8192 - 1 - 32);
}

#[test]
fn claimable_shows_the_span_without_taking_it() {
    let plan = pool("10.200.0.0/16");
    let seen = plan.claimable(24).expect("a span");
    assert_eq!(seen.cidr(), cidr("10.200.0.0/24"));
    assert_eq!(plan.available(), 8192, "nothing is taken");
    assert_eq!(plan.claim_span(24), Some(seen));
    assert_eq!(
        plan.claimable(24).map(|s| s.cidr()),
        Some(cidr("10.200.1.0/24"))
    );
}

#[test]
fn a_pool_that_cannot_hold_the_span_and_one_run_block_shrinks_it() {
    for (text, taken, span, left) in [
        // A /24 pool cannot hold a /24 and a run block: half of it.
        ("10.200.0.0/24", vec![], "10.200.0.0/25", 16),
        // Two blocks: one each.
        ("10.200.0.0/28", vec![], "10.200.0.0/29", 1),
        // The free aligned /28 is the second one.
        ("10.200.0.0/27", vec!["10.200.0.0/29"], "10.200.0.16/28", 1),
        // A taken block in each half of a /26: a /28 still fits.
        (
            "10.200.0.0/26",
            vec!["10.200.0.0/29", "10.200.0.32/29"],
            "10.200.0.16/28",
            4,
        ),
    ] {
        let plan = pool(text);
        plan.exclude(&taken.iter().map(|t| cidr(t)).collect::<Vec<_>>());
        let got = plan.claim_span(24).expect("a smaller span");
        assert_eq!(got.cidr(), cidr(span), "{text}");
        assert_eq!(plan.available(), left, "{text}");
    }
}

#[test]
fn a_pool_without_room_for_a_29_and_one_run_block_claims_nothing() {
    for (text, taken) in [
        ("10.200.0.0/29", vec![]),
        ("10.200.0.0/28", vec!["10.200.0.8/29"]),
    ] {
        let plan = pool(text);
        plan.exclude(&taken.iter().map(|t| cidr(t)).collect::<Vec<_>>());
        assert!(plan.claimable(24).is_none(), "{text}");
        assert!(plan.claim_span(24).is_none(), "{text}");
        assert_eq!(plan.available(), 1, "{text}");
    }
}

#[test]
fn a_span_larger_than_the_pool_is_never_asked_for() {
    // Asking for a /16 from a /24 pool gives the largest span that leaves a run block.
    let plan = pool("10.200.0.0/24");
    assert_eq!(
        plan.claim_span(16).map(|s| s.cidr()),
        Some(cidr("10.200.0.0/25"))
    );
}

// GatewayBridge::plan_network ---------------------------------------------------------------------

#[tokio::test]
async fn a_missing_kestrel_gw_gets_a_24_of_the_plan() {
    let runner = docker("");
    let plan_ = pool("10.200.0.0/16");
    let got = plan(&runner, &plan_).await;
    assert_eq!(span_of(&got.network), cidr("10.200.0.0/24"));
    assert!(got.warning.is_none());
    assert_eq!(plan_.available(), 8192 - 32);
    assert_eq!(runner.runs(), vec![lookup()], "only the lookup runs");
}

#[tokio::test]
async fn a_missing_kestrel_gw_in_a_small_pool_gets_a_smaller_span() {
    let runner = docker("");
    let plan_ = pool("10.200.0.0/24");
    let got = plan(&runner, &plan_).await;
    assert_eq!(span_of(&got.network), cidr("10.200.0.0/25"));
    assert_eq!(plan_.available(), 16);
}

#[tokio::test]
async fn an_existing_kestrel_gw_as_large_as_the_span_is_reused_and_takes_nothing() {
    for gw in ["3;172.30.0.0/24 ", "0;172.30.0.0/16 fd00::/64 "] {
        let runner = docker_with("kestrel-gw\n", gw);
        let plan_ = pool("10.200.0.0/16");
        let got = plan(&runner, &plan_).await;
        assert_eq!(got.network, BridgeNetwork::Existing, "{gw}");
        assert!(got.warning.is_none(), "{gw}");
        assert_eq!(plan_.available(), 8192);
        assert!(!runner.runs().contains(&rm_gw()), "{gw}");
    }
}

#[tokio::test]
async fn an_unreadable_kestrel_gw_is_reused_as_it_is() {
    for gw in ["", "garbage", "x;10.200.0.0/29", "0;", "0;not-a-cidr "] {
        let runner = docker_with("kestrel-gw\n", gw);
        let plan_ = pool("10.200.0.0/16");
        let got = plan(&runner, &plan_).await;
        assert_eq!(got.network, BridgeNetwork::Existing, "{gw:?}");
        assert!(!runner.runs().contains(&rm_gw()), "{gw:?}");
        assert_eq!(plan_.available(), 8192);
    }
}

#[tokio::test]
async fn only_a_network_named_exactly_kestrel_gw_counts_as_existing() {
    // A run network's name contains `kestrel-gw` too.
    let runner = docker("kestrel-gw-b302099a8057\n");
    let plan_ = pool("10.200.0.0/16");
    let got = plan(&runner, &plan_).await;
    assert!(
        matches!(got.network, BridgeNetwork::Subnet(_)),
        "{:?}",
        got.network
    );
}

#[tokio::test]
async fn a_smaller_kestrel_gw_with_nothing_attached_is_recreated_on_the_span() {
    let runner = docker_with("kestrel-gw\n", "0;172.30.0.0/29 ");
    let plan_ = pool("10.200.0.0/16");
    let got = plan(&runner, &plan_).await;
    assert_eq!(span_of(&got.network), cidr("10.200.0.0/24"));
    assert!(got.warning.is_none());
    assert_eq!(runner.runs(), vec![lookup(), inspect_gw(), rm_gw()]);
    assert_eq!(plan_.available(), 8192 - 32);
}

#[tokio::test]
async fn a_smaller_kestrel_gw_with_containers_attached_is_kept_with_a_warning() {
    let runner = docker_with("kestrel-gw\n", "2;172.30.0.0/29 ");
    let plan_ = pool("10.200.0.0/16");
    let got = plan(&runner, &plan_).await;
    assert_eq!(got.network, BridgeNetwork::Existing);
    let warning = got.warning.expect("a warning");
    for needle in [
        "kestrel-gw",
        "172.30.0.0/29",
        "docker network rm kestrel-gw",
    ] {
        assert!(warning.contains(needle), "{needle}: {warning}");
    }
    assert!(!runner.runs().contains(&rm_gw()));
    assert_eq!(plan_.available(), 8192, "nothing is claimed");
}

#[tokio::test]
async fn a_smaller_kestrel_gw_that_cannot_be_removed_is_kept_with_a_warning() {
    let runner = FakeRunner::with(|argv| {
        if argv == lookup().as_slice() {
            out(0, "kestrel-gw\n", "")
        } else if argv == inspect_gw().as_slice() {
            out(0, "0;172.30.0.0/29 ", "")
        } else if argv == rm_gw().as_slice() {
            out(
                1,
                "",
                "Error response from daemon: network has active endpoints",
            )
        } else {
            out(0, "", "")
        }
    });
    let plan_ = pool("10.200.0.0/16");
    let got = plan(&runner, &plan_).await;
    assert_eq!(got.network, BridgeNetwork::Existing);
    let warning = got.warning.expect("a warning");
    assert!(warning.contains("active endpoints"), "{warning}");
    assert!(warning.contains("172.30.0.0/29"), "{warning}");
    assert_eq!(plan_.available(), 8192, "nothing is claimed");
}

#[tokio::test]
async fn a_kestrel_gw_as_large_as_the_pool_allows_is_kept() {
    // The pool offers only a /29 for the bridge, and kestrel-gw is a /29 already.
    let runner = docker_with("kestrel-gw\n", "0;172.30.0.0/29 ");
    let plan_ = pool("10.200.0.0/28");
    let got = plan(&runner, &plan_).await;
    assert_eq!(got.network, BridgeNetwork::Existing);
    assert!(got.warning.is_none());
    assert!(!runner.runs().contains(&rm_gw()));
    assert_eq!(plan_.available(), 2);
}

#[tokio::test]
async fn a_pool_without_room_for_the_bridge_and_one_run_fails_naming_the_setting() {
    for (text, taken) in [
        ("10.200.0.0/29", None),
        ("10.200.0.0/28", Some("10.200.0.8/29")),
    ] {
        let runner = docker("");
        let plan_ = pool(text);
        plan_.exclude(&taken.map(cidr).into_iter().collect::<Vec<_>>());
        let e = GatewayBridge::plan_network(&*runner, &plan_)
            .await
            .expect_err("too small");
        let text_of = e.to_string();
        assert!(text_of.contains("gateway.subnet_pool"), "{text_of}");
        assert!(text_of.contains(text), "names the pool: {text_of}");
        assert!(text_of.contains("kestrel-gw"), "{text_of}");
        assert_eq!(plan_.available(), 1, "nothing is claimed: {text_of}");
    }
}

#[tokio::test]
async fn an_existing_kestrel_gw_needs_no_room_in_the_pool() {
    let runner = docker_with("kestrel-gw\n", "1;172.30.0.0/24 ");
    let plan_ = pool("10.200.0.0/29");
    let got = plan(&runner, &plan_).await;
    assert_eq!(got.network, BridgeNetwork::Existing);
    assert_eq!(plan_.available(), 1);
}

#[tokio::test]
async fn a_docker_that_cannot_list_or_read_networks_fails_the_lookup() {
    let runner = FakeRunner::with(|_| out(1, "", "Cannot connect to the Docker daemon"));
    let failing_inspect = FakeRunner::with(|argv| {
        if argv == lookup().as_slice() {
            out(0, "kestrel-gw\n", "")
        } else {
            out(1, "", "Cannot connect to the Docker daemon")
        }
    });
    for runner in [runner, failing_inspect] {
        let plan_ = pool("10.200.0.0/16");
        let e = GatewayBridge::plan_network(&*runner, &plan_)
            .await
            .expect_err("docker is down");
        assert!(
            matches!(&e, SandboxError::Bridge { step: "create-network", stderr } if stderr.contains("Cannot connect")),
            "{e:?}"
        );
        assert_eq!(plan_.available(), 8192);
    }
}

// GatewayBridge::start ----------------------------------------------------------------------------

#[tokio::test]
async fn the_network_create_carries_the_planned_span_and_gateway() {
    let runner = docker("");
    let plan_ = pool("10.200.0.0/16");
    let got = plan(&runner, &plan_).await;
    let bridge = start(&runner, got.network).await;
    assert_eq!(
        creates(&runner),
        vec![s(&[
            "docker",
            "network",
            "create",
            "--internal",
            "--subnet",
            "10.200.0.0/24",
            "--gateway",
            "10.200.0.1",
            "kestrel-gw",
        ])]
    );
    assert_eq!(bridge.url().as_str(), "http://10.200.0.2:7070/");
    assert_eq!(bridge.name(), name());
}

#[tokio::test]
async fn an_existing_kestrel_gw_is_joined_without_a_create() {
    let runner = docker_with("kestrel-gw\n", "1;172.30.0.0/24 ");
    let plan_ = pool("10.200.0.0/16");
    let got = plan(&runner, &plan_).await;
    start(&runner, got.network).await;
    assert!(creates(&runner).is_empty(), "{:?}", runner.runs());
    assert!(
        runner
            .runs()
            .contains(&s(&["docker", "network", "connect", "kestrel-gw", &name()])),
        "{:?}",
        runner.runs()
    );
}

#[tokio::test]
async fn kestrel_gw_made_by_someone_else_after_the_lookup_is_used_as_it_is() {
    let runner = FakeRunner::with(|argv| {
        if argv.get(2).map(String::as_str) == Some("create") {
            out(
                1,
                "",
                "Error response from daemon: network with name kestrel-gw already exists",
            )
        } else if argv.get(1).map(String::as_str) == Some("inspect") {
            out(0, IP, "")
        } else {
            out(0, "", "")
        }
    });
    let plan_ = pool("10.200.0.0/16");
    let span = plan_.claim_span(24).expect("a span");
    start(&runner, BridgeNetwork::Subnet(span)).await;
}

#[tokio::test]
async fn a_failing_create_on_the_planned_span_is_a_create_network_error() {
    let runner = FakeRunner::with(|argv| {
        if argv.get(2).map(String::as_str) == Some("create") {
            out(
                1,
                "",
                "Error response from daemon: Pool overlaps with other one",
            )
        } else {
            out(0, "", "")
        }
    });
    let plan_ = pool("10.200.0.0/16");
    let span = plan_.claim_span(24).expect("a span");
    let spec = BridgeSpec {
        network: BridgeNetwork::Subnet(span),
        ..BridgeSpec::default()
    };
    let e = GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
        .await
        .expect_err("create fails");
    assert!(
        matches!(&e, SandboxError::Bridge { step: "create-network", stderr } if stderr.contains("Pool overlaps")),
        "{e:?}"
    );
    assert!(
        !runner
            .runs()
            .iter()
            .any(|a| a.get(1).map(String::as_str) == Some("run")),
        "no container is started"
    );
}

#[tokio::test]
async fn a_full_kestrel_gw_at_connect_says_what_to_do() {
    let connect = s(&["docker", "network", "connect", "kestrel-gw", &name()]);
    let runner = FakeRunner::with(move |argv| {
        if argv == connect.as_slice() {
            out(
                1,
                "",
                "Error response from daemon: no available IPv4 addresses on this network's address pools: kestrel-gw (5f1e2d3c4b5a)",
            )
        } else {
            out(0, "", "")
        }
    });
    let spec = BridgeSpec {
        network: BridgeNetwork::Existing,
        ..BridgeSpec::default()
    };
    let e = GatewayBridge::start(runner.clone(), 7070, spec, Path::new(DB))
        .await
        .expect_err("kestrel-gw is full");
    let SandboxError::Bridge {
        step: "connect",
        stderr,
    } = &e
    else {
        panic!("expected the connect step: {e:?}");
    };
    for needle in [
        "kestrel-gw is full",
        "kestrel-gateway-",
        "docker ps -a --filter label=kestrel.managed=bridge",
        "docker network rm kestrel-gw",
        "no available IPv4 addresses",
    ] {
        assert!(stderr.contains(needle), "{needle}: {stderr}");
    }
    assert!(
        runner
            .runs()
            .contains(&s(&["docker", "rm", "--force", &name()])),
        "the half-made bridge is removed: {:?}",
        runner.runs()
    );
}

#[tokio::test]
async fn other_connect_errors_are_passed_on_as_they_are() {
    let connect = s(&["docker", "network", "connect", "kestrel-gw", &name()]);
    let runner = FakeRunner::with(move |argv| {
        if argv == connect.as_slice() {
            out(
                1,
                "",
                "Error response from daemon: network kestrel-gw not found",
            )
        } else {
            out(0, "", "")
        }
    });
    let spec = BridgeSpec {
        network: BridgeNetwork::Existing,
        ..BridgeSpec::default()
    };
    let e = GatewayBridge::start(runner, 7070, spec, Path::new(DB))
        .await
        .expect_err("connect fails");
    assert!(
        matches!(&e, SandboxError::Bridge { step: "connect", stderr } if stderr == "Error response from daemon: network kestrel-gw not found"),
        "{e:?}"
    );
}

#[test]
fn the_default_network_is_docker_pools_as_before() {
    assert_eq!(BridgeSpec::default().network, BridgeNetwork::DockerPools);
    assert_eq!(BridgeNetwork::default(), BridgeNetwork::DockerPools);
}
