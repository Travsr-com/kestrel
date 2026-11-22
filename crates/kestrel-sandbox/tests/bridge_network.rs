//! Issue #99: the bridge's own `kestrel-gw` network gets an explicit subnet from Kestrel's plan,
//! so serve starts on a host whose docker default address pools are used up.
//!
//! * `SubnetPool::claim` takes a block out of the plan for good: no run network ever gets it.
//! * `GatewayBridge::plan_network` reuses an existing `kestrel-gw` as it is, and otherwise claims
//!   the lowest free block, refusing a pool that cannot also hold one run block.
//! * `GatewayBridge::start` creates `kestrel-gw` with `--subnet` and `--gateway` from the claimed
//!   block, and never runs `network create` for an existing one.

#![allow(clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{FakeRunner, s};
use kestrel_sandbox::{
    BridgeNetwork, BridgeSpec, CommandOutput, GatewayBridge, Ipv4Cidr, SandboxError, SubnetPool,
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

fn out(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
    CommandOutput {
        status,
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
    }
}

/// Docker that answers `lookup` with `listed`, the bridge's address on `kestrel-gw` with [`IP`],
/// and succeeds otherwise.
fn docker(listed: &'static str) -> Arc<FakeRunner> {
    FakeRunner::with(move |argv| {
        if argv == lookup().as_slice() {
            out(0, listed, "")
        } else if argv.get(1).map(String::as_str) == Some("inspect") {
            out(0, &format!("{IP}\n"), "")
        } else {
            out(0, "", "")
        }
    })
}

fn creates(runner: &FakeRunner) -> Vec<Vec<String>> {
    runner
        .runs()
        .into_iter()
        .filter(|a| a.get(1).map(String::as_str) == Some("network") && a[2] == "create")
        .collect()
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

// SubnetPool::claim -------------------------------------------------------------------------------

#[test]
fn claim_takes_the_lowest_free_block_out_of_the_plan_for_good() {
    let plan = pool("10.200.0.0/28");
    let claimed = plan.claim().expect("a free block");
    assert_eq!(claimed.cidr(), cidr("10.200.0.0/29"));
    assert_eq!(claimed.gateway().to_string(), "10.200.0.1");
    assert_eq!(plan.available(), 1);
    assert_eq!(plan.in_use(), 0, "a claimed block is not a run's block");

    // Releasing the claimed block, as a run would, never brings it back.
    plan.release(&claimed);
    assert_eq!(plan.available(), 1);
    let run = plan.reserve().expect("the other block");
    assert_eq!(run.cidr(), cidr("10.200.0.8/29"));
    assert!(plan.reserve().is_none());
    plan.release(&run);
    assert_eq!(
        plan.reserve().map(|r| r.cidr()),
        Some(cidr("10.200.0.8/29"))
    );
}

#[test]
fn run_networks_never_get_the_bridges_block() {
    let plan = pool("10.200.0.0/24");
    plan.exclude(&[cidr("10.200.0.0/29")]);
    let claimed = plan.claim().expect("a free block");
    assert_eq!(claimed.cidr(), cidr("10.200.0.8/29"), "skips taken blocks");
    let mut runs = Vec::new();
    while let Some(run) = plan.reserve() {
        assert!(!run.cidr().overlaps(&claimed.cidr()), "{:?}", run.cidr());
        runs.push(run);
    }
    assert_eq!(runs.len(), 30);
    for run in &runs {
        plan.release(run);
    }
    assert_eq!(plan.available(), 30);
}

#[test]
fn a_pool_whose_only_free_block_is_claimed_is_exhausted() {
    let plan = pool("10.200.0.0/29");
    assert!(plan.claim().is_some());
    assert_eq!(plan.available(), 0);
    assert!(plan.exhausted(), "no run can ever get a block");
    assert!(plan.claim().is_none(), "nothing left to claim");
}

// GatewayBridge::plan_network ---------------------------------------------------------------------

#[tokio::test]
async fn a_missing_kestrel_gw_gets_the_lowest_free_block_of_the_plan() {
    let runner = docker("");
    let plan = pool("10.200.0.0/16");
    plan.exclude(&[cidr("10.200.0.0/29")]);
    let network = GatewayBridge::plan_network(&*runner, &plan)
        .await
        .expect("planned");
    let BridgeNetwork::Subnet(subnet) = network else {
        panic!("expected a block, got {network:?}");
    };
    assert_eq!(subnet.cidr(), cidr("10.200.0.8/29"));
    assert_eq!(plan.available(), 8190);
    assert_eq!(runner.runs(), vec![lookup()], "only the lookup runs");
}

#[tokio::test]
async fn an_existing_kestrel_gw_is_reused_and_takes_no_block() {
    let runner = docker("kestrel-gw\n");
    let plan = pool("10.200.0.0/16");
    let network = GatewayBridge::plan_network(&*runner, &plan)
        .await
        .expect("planned");
    assert_eq!(network, BridgeNetwork::Existing);
    assert_eq!(plan.available(), 8192);
}

#[tokio::test]
async fn only_a_network_named_exactly_kestrel_gw_counts_as_existing() {
    // A run network's name contains `kestrel-gw` too.
    let runner = docker("kestrel-gw-b302099a8057\n");
    let plan = pool("10.200.0.0/16");
    let network = GatewayBridge::plan_network(&*runner, &plan)
        .await
        .expect("planned");
    assert!(matches!(network, BridgeNetwork::Subnet(_)), "{network:?}");
}

#[tokio::test]
async fn a_pool_without_room_for_the_bridge_and_one_run_fails_naming_the_setting() {
    for (text, taken) in [
        ("10.200.0.0/29", None),
        ("10.200.0.0/28", Some("10.200.0.8/29")),
    ] {
        let runner = docker("");
        let plan = pool(text);
        plan.exclude(&taken.map(cidr).into_iter().collect::<Vec<_>>());
        let e = GatewayBridge::plan_network(&*runner, &plan)
            .await
            .expect_err("too small");
        let text_of = e.to_string();
        assert!(text_of.contains("gateway.subnet_pool"), "{text_of}");
        assert!(text_of.contains(text), "names the pool: {text_of}");
        assert!(text_of.contains("kestrel-gw"), "{text_of}");
        assert_eq!(plan.available(), 1, "nothing is claimed: {text_of}");
    }
}

#[tokio::test]
async fn an_existing_kestrel_gw_needs_no_room_in_the_pool() {
    let runner = docker("kestrel-gw\n");
    let plan = pool("10.200.0.0/29");
    let network = GatewayBridge::plan_network(&*runner, &plan)
        .await
        .expect("planned");
    assert_eq!(network, BridgeNetwork::Existing);
    assert_eq!(plan.available(), 1);
}

#[tokio::test]
async fn a_docker_that_cannot_list_networks_fails_the_lookup() {
    let runner = FakeRunner::with(|_| out(1, "", "Cannot connect to the Docker daemon"));
    let plan = pool("10.200.0.0/16");
    let e = GatewayBridge::plan_network(&*runner, &plan)
        .await
        .expect_err("docker is down");
    assert!(
        matches!(&e, SandboxError::Bridge { step: "create-network", stderr } if stderr.contains("Cannot connect")),
        "{e:?}"
    );
    assert_eq!(plan.available(), 8192);
}

// GatewayBridge::start ----------------------------------------------------------------------------

#[tokio::test]
async fn the_network_create_carries_the_planned_subnet_and_gateway() {
    let runner = docker("");
    let plan = pool("10.200.0.0/16");
    let network = GatewayBridge::plan_network(&*runner, &plan)
        .await
        .expect("planned");
    let bridge = start(&runner, network).await;
    assert_eq!(
        creates(&runner),
        vec![s(&[
            "docker",
            "network",
            "create",
            "--internal",
            "--subnet",
            "10.200.0.0/29",
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
    let runner = docker("kestrel-gw\n");
    let plan = pool("10.200.0.0/16");
    let network = GatewayBridge::plan_network(&*runner, &plan)
        .await
        .expect("planned");
    start(&runner, network).await;
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
    let plan = pool("10.200.0.0/16");
    let subnet = plan.claim().expect("a block");
    start(&runner, BridgeNetwork::Subnet(subnet)).await;
}

#[tokio::test]
async fn a_failing_create_on_the_planned_block_is_a_create_network_error() {
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
    let plan = pool("10.200.0.0/16");
    let subnet = plan.claim().expect("a block");
    let spec = BridgeSpec {
        network: BridgeNetwork::Subnet(subnet),
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

#[test]
fn the_default_network_is_docker_pools_as_before() {
    assert_eq!(BridgeSpec::default().network, BridgeNetwork::DockerPools);
    assert_eq!(BridgeNetwork::default(), BridgeNetwork::DockerPools);
}
