//! Edges of run networks and the subnet plan that the scenario tests do not reach: names that
//! never get to the CLI, a docker that cannot be started, route types in `ip route` output, and
//! a network removed between `docker network ls` and `docker network inspect`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{FakeRunner, s};
use kestrel_sandbox::{
    CommandOutput, CommandRunner, GvisorDriver, Ipv4Cidr, NetworkPolicy, PreparedNetwork,
    SandboxDriver, SandboxError, SpawnedProcess, SubnetPool, docker_subnets, host_routes,
};
use kestrel_types::RunId;
use url::Url;

fn ok(stdout: &str) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

fn cidr(text: &str) -> Ipv4Cidr {
    Ipv4Cidr::parse(text).expect("cidr")
}

fn block() -> kestrel_sandbox::Subnet {
    SubnetPool::parse("10.200.0.0/29")
        .expect("pool")
        .reserve()
        .expect("block")
}

/// A runner whose processes never start.
struct NoDocker;

#[async_trait::async_trait]
impl CommandRunner for NoDocker {
    async fn run(&self, _: &[String]) -> std::io::Result<CommandOutput> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no docker",
        ))
    }
    async fn spawn(&self, _: &[String]) -> std::io::Result<SpawnedProcess> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no docker",
        ))
    }
}

#[tokio::test]
async fn a_bridge_name_docker_would_read_as_an_option_is_refused_before_any_command() {
    let runner = FakeRunner::ok();
    let err = GvisorDriver::new(runner.clone())
        .with_bridge("--rm")
        .prepare_network(RunId::new(), &block())
        .await
        .expect_err("refused");
    assert!(matches!(err, SandboxError::InvalidId { .. }), "{err}");
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn a_run_network_name_that_is_not_a_name_is_refused_by_create_and_release() {
    let runner = FakeRunner::ok();
    let driver = GvisorDriver::new(runner.clone()).with_bridge("kestrel-gateway-1");
    let mut spec = common::spec();
    spec.workspace = None;
    spec.network = NetworkPolicy::RunNetwork {
        network: "-x".into(),
        gateway: Url::parse("http://10.200.0.2:7070/").expect("url"),
    };
    assert!(driver.create(&spec).await.is_err());
    let bad = PreparedNetwork {
        name: "a b".into(),
        gateway_host: "10.200.0.2".parse().expect("ip"),
        subnet: block(),
    };
    assert!(driver.release_network(&bad).await.is_err());
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn a_docker_that_cannot_start_fails_the_create_step_by_name() {
    let err = GvisorDriver::new(Arc::new(NoDocker))
        .with_bridge("kestrel-gateway-1")
        .prepare_network(RunId::new(), &block())
        .await
        .expect_err("no docker");
    let msg = err.to_string();
    assert!(msg.contains("create") && msg.contains("no docker"), "{msg}");
    assert!(docker_subnets(&NoDocker).await.is_err());
}

#[tokio::test]
async fn sweep_without_a_bridge_only_removes_and_reports_the_first_failure() {
    let runner = FakeRunner::with(|argv| {
        if argv.get(2).map(String::as_str) == Some("ls") {
            return ok("kestrel-gw-aaaaaaaaaaaa\nkestrel-gw-bbbbbbbbbbbb\n");
        }
        CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "Cannot connect to the Docker daemon".into(),
        }
    });
    let err = GvisorDriver::new(runner.clone())
        .sweep_networks()
        .await
        .expect_err("rm failed");
    assert!(err.to_string().contains("Cannot connect"), "{err}");
    let runs = runner.runs();
    assert_eq!(
        &runs[1..],
        &[
            s(&["docker", "network", "rm", "kestrel-gw-aaaaaaaaaaaa"]),
            s(&["docker", "network", "rm", "kestrel-gw-bbbbbbbbbbbb"]),
        ],
        "every network is tried, without a disconnect"
    );
}

#[tokio::test]
async fn a_bad_instance_id_is_refused_by_prepare_and_sweep() {
    let runner = FakeRunner::ok();
    let driver = GvisorDriver::new(runner.clone())
        .with_instance("a b")
        .with_bridge("kestrel-gateway-1");
    assert!(
        driver
            .prepare_network(RunId::new(), &block())
            .await
            .is_err()
    );
    assert!(driver.sweep_networks().await.is_err());
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn docker_subnets_keeps_what_docker_printed_for_networks_that_still_exist() {
    let runner = FakeRunner::with(|argv| {
        if argv.get(2).map(String::as_str) == Some("ls") {
            return ok("1a2b3c4d5e6f\n--format\n2b3c4d5e6f70\n");
        }
        CommandOutput {
            status: 1,
            stdout: "10.9.0.0/16 \n".into(),
            stderr: "Error: No such network: 2b3c4d5e6f70".into(),
        }
    });
    let got = docker_subnets(runner.as_ref()).await.expect("subnets");
    assert_eq!(got, vec![cidr("10.9.0.0/16")]);
    let inspect = &runner.runs()[1];
    assert_eq!(
        inspect.iter().filter(|a| a.as_str() == "--format").count(),
        1,
        "an id that looks like an option is dropped: {inspect:?}"
    );
}

#[tokio::test]
async fn host_routes_read_route_types_and_single_addresses() {
    let runner = FakeRunner::with(|_| {
        ok("unreachable 10.200.0.0/24 proto kernel\n\
            10.1.2.3 via 192.168.1.1 dev eth0\n\
            blackhole\n\
            10.7.0.5/16 dev x\n\
            garbage line\n")
    });
    let mut got = host_routes(runner.as_ref()).await;
    got.sort();
    assert_eq!(
        got,
        vec![
            cidr("10.1.2.3/32"),
            cidr("10.7.0.0/16"),
            cidr("10.200.0.0/24")
        ]
    );
}

#[test]
fn a_pool_shows_its_range_and_free_blocks_when_debug_printed() {
    let pool = SubnetPool::parse("10.200.0.0/28").expect("pool");
    let text = format!("{pool:?}");
    assert!(
        text.contains("10.200.0.0") && text.contains("available: 2"),
        "{text}"
    );
}

#[test]
fn a_released_block_just_below_the_pool_changes_nothing() {
    let pool = SubnetPool::parse("10.200.0.0/28").expect("pool");
    let a = pool.reserve().expect("a");
    let below = SubnetPool::parse("10.199.255.248/29")
        .expect("below")
        .reserve()
        .expect("block");
    pool.release(&below);
    assert_eq!(pool.available(), 1);
    pool.release(&a);
    assert_eq!(pool.available(), 2);
}

#[test]
fn exclusions_that_overlap_or_touch_merge() {
    let pool = SubnetPool::parse("10.200.0.0/26").expect("eight blocks");
    pool.exclude(&[cidr("10.200.0.8/29"), cidr("10.200.0.24/29")]);
    pool.exclude(&[cidr("10.200.0.16/29"), cidr("10.200.0.8/30")]);
    assert_eq!(pool.available(), 5);
    let got: Vec<String> = std::iter::from_fn(|| pool.reserve())
        .map(|s| s.cidr().to_string())
        .collect();
    assert_eq!(
        got,
        vec![
            "10.200.0.0/29",
            "10.200.0.32/29",
            "10.200.0.40/29",
            "10.200.0.48/29",
            "10.200.0.56/29"
        ]
    );
}
