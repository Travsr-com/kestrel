//! Issues #10 and #34: one internal network per run, with the shared gateway bridge joined to it.
//! Exact docker argv for `prepare_network`, `release_network`, `sweep_networks` and `create` with
//! `NetworkPolicy::RunNetwork`, cleanup after a failing step, idempotent teardown, the "Pool
//! overlaps" answer reported as a taken subnet, and the defaults of drivers without a bridge.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use common::{FakeRunner, ID, s, spec};
use kestrel_sandbox::{
    AppleContainerDriver, CommandOutput, DockerRuncDriver, ExitStatus, GvisorDriver, LaunchSpec,
    NetworkPolicy, OutputStream, PreparedNetwork, SandboxDriver, SandboxHandle, Subnet, SubnetPool,
};
use kestrel_types::RunId;
use url::Url;

const RUN: &str = "01890a5d-ac96-774b-bcce-b302099a8057";
const NET: &str = "kestrel-gw-b302099a8057";
const BRIDGE: &str = "kestrel-gateway-b29b95f5";
const BRIDGE_IP: &str = "10.200.0.2";
const OVERLAP: &str =
    "Error response from daemon: Pool overlaps with other one on this address space";

fn run_id() -> RunId {
    RUN.parse().expect("run id")
}

/// The first block of the default pool: `10.200.0.0/29`, gateway `10.200.0.1`.
fn first_block() -> Subnet {
    SubnetPool::parse("10.200.0.0/16")
        .expect("pool")
        .reserve()
        .expect("a block")
}

fn create_net(instance: Option<&str>) -> Vec<String> {
    let mut argv = s(&[
        "docker",
        "network",
        "create",
        "--internal",
        "-o",
        "com.docker.network.bridge.inhibit_ipv4=true",
        "--subnet",
        "10.200.0.0/29",
        "--gateway",
        "10.200.0.1",
        "--label",
        &format!("kestrel.run={RUN}"),
    ]);
    if let Some(id) = instance {
        argv.extend(s(&["--label", &format!("kestrel.instance={id}")]));
    }
    argv.push(NET.to_string());
    argv
}

fn connect() -> Vec<String> {
    s(&["docker", "network", "connect", NET, BRIDGE])
}

fn inspect() -> Vec<String> {
    s(&[
        "docker",
        "container",
        "inspect",
        "--format",
        &format!("{{{{(index .NetworkSettings.Networks \"{NET}\").IPAddress}}}}"),
        BRIDGE,
    ])
}

fn disconnect(net: &str) -> Vec<String> {
    s(&["docker", "network", "disconnect", "--force", net, BRIDGE])
}

fn rm(net: &str) -> Vec<String> {
    s(&["docker", "network", "rm", net])
}

fn ok(stdout: &str) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

fn failed(stderr: &str) -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

/// Docker with the bridge running: every call works and `inspect` prints the bridge IP.
fn docker() -> Arc<FakeRunner> {
    docker_with(|_| None)
}

/// Like [`docker`], but `answer(argv)` overrides the reply when it returns `Some`.
fn docker_with(
    answer: impl Fn(&[String]) -> Option<CommandOutput> + Send + Sync + 'static,
) -> Arc<FakeRunner> {
    FakeRunner::with(move |argv| {
        if let Some(reply) = answer(argv) {
            return reply;
        }
        if argv == inspect().as_slice() {
            return ok(&format!("{BRIDGE_IP}\n"));
        }
        ok(&format!("{ID}\n"))
    })
}

fn gvisor(runner: &Arc<FakeRunner>) -> GvisorDriver {
    GvisorDriver::new(runner.clone()).with_bridge(BRIDGE)
}

fn prepared() -> PreparedNetwork {
    PreparedNetwork {
        name: NET.to_string(),
        gateway_host: BRIDGE_IP.parse().expect("ip"),
        subnet: first_block(),
    }
}

fn is(argv: &[String], prefix: &[&str]) -> bool {
    argv.len() >= prefix.len() && argv.iter().zip(prefix).all(|(a, p)| a == p)
}

// prepare_network ---------------------------------------------------------------------------------

#[tokio::test]
async fn prepare_creates_an_internal_labelled_network_joins_the_bridge_and_reads_its_ip() {
    let runner = docker();
    let net = gvisor(&runner)
        .prepare_network(run_id(), &first_block())
        .await
        .expect("prepared")
        .expect("a bridge means a run network");

    assert_eq!(runner.runs(), vec![create_net(None), connect(), inspect()]);
    assert_eq!(
        net,
        PreparedNetwork {
            name: NET.to_string(),
            gateway_host: BRIDGE_IP.parse::<Ipv4Addr>().expect("ip"),
            subnet: first_block(),
        }
    );
}

#[tokio::test]
async fn prepare_labels_the_network_with_the_instance_id() {
    let runner = docker();
    gvisor(&runner)
        .with_instance("1a2b3c4d")
        .prepare_network(run_id(), &first_block())
        .await
        .expect("prepared")
        .expect("some");
    assert_eq!(runner.runs()[0], create_net(Some("1a2b3c4d")));
}

#[tokio::test]
async fn docker_runc_prepares_the_same_way() {
    let runner = docker();
    let net = DockerRuncDriver::new(runner.clone())
        .with_bridge(BRIDGE)
        .prepare_network(run_id(), &first_block())
        .await
        .expect("prepared");
    assert!(net.is_some());
    assert_eq!(runner.runs(), vec![create_net(None), connect(), inspect()]);
}

#[tokio::test]
async fn two_runs_get_two_networks_each_with_its_own_bridge_ip() {
    let other: RunId = "01890a5d-ac96-774b-bcce-0000000000aa".parse().expect("id");
    let other_net = "kestrel-gw-0000000000aa";
    let runner = docker_with(move |argv| {
        let wanted = format!("{{{{(index .NetworkSettings.Networks \"{other_net}\").IPAddress}}}}");
        argv.contains(&wanted).then(|| ok("10.200.0.10\n"))
    });
    let pool = SubnetPool::parse("10.200.0.0/16").expect("pool");
    let driver = gvisor(&runner);
    let a = driver
        .prepare_network(run_id(), &pool.reserve().expect("a"))
        .await
        .expect("a")
        .expect("some");
    let b = driver
        .prepare_network(other, &pool.reserve().expect("b"))
        .await
        .expect("b")
        .expect("some");

    assert_eq!(a.name, NET);
    assert_eq!(b.name, other_net);
    assert_eq!(a.gateway_host, BRIDGE_IP.parse::<Ipv4Addr>().expect("ip"));
    assert_eq!(
        b.gateway_host,
        "10.200.0.10".parse::<Ipv4Addr>().expect("ip")
    );
    assert_ne!(a.subnet, b.subnet);
    let creates: Vec<_> = runner
        .runs()
        .into_iter()
        .filter(|a| is(a, &["docker", "network", "create"]))
        .collect();
    assert_eq!(creates.len(), 2);
    assert!(
        creates[1].contains(&"10.200.0.8/29".to_string()),
        "{creates:?}"
    );
    assert!(
        creates[1].contains(&"10.200.0.9".to_string()),
        "{creates:?}"
    );
}

#[tokio::test]
async fn pool_overlaps_from_docker_is_reported_as_a_taken_subnet() {
    let runner =
        docker_with(|argv| is(argv, &["docker", "network", "create"]).then(|| failed(OVERLAP)));
    let err = gvisor(&runner)
        .prepare_network(run_id(), &first_block())
        .await
        .expect_err("taken");
    let msg = err.to_string();
    assert!(msg.to_lowercase().contains("taken"), "{msg}");
    assert!(msg.contains("10.200.0.0/29"), "{msg}");
    assert!(
        !runner
            .runs()
            .iter()
            .any(|a| a == &connect() || a == &inspect()),
        "nothing after a failed create: {:?}",
        runner.runs()
    );
}

#[tokio::test]
async fn a_failed_network_create_fails_naming_the_step_and_joins_nothing() {
    let runner = docker_with(|argv| {
        is(argv, &["docker", "network", "create"]).then(|| failed("permission denied"))
    });
    let err = gvisor(&runner)
        .prepare_network(run_id(), &first_block())
        .await
        .expect_err("create failed");
    let msg = err.to_string();
    assert!(msg.contains("create"), "{msg}");
    assert!(!msg.to_lowercase().contains("taken"), "{msg}");
    assert!(
        !runner
            .runs()
            .iter()
            .any(|a| a == &connect() || a == &inspect()),
        "{:?}",
        runner.runs()
    );
}

#[tokio::test]
async fn a_failed_connect_removes_the_network_and_names_the_step() {
    let runner =
        docker_with(|argv| (argv == connect().as_slice()).then(|| failed("no such container")));
    let err = gvisor(&runner)
        .prepare_network(run_id(), &first_block())
        .await
        .expect_err("connect failed");
    assert!(err.to_string().contains("connect"), "{err}");
    let runs = runner.runs();
    assert_eq!(&runs[..2], &[create_net(None), connect()]);
    assert!(!runs.contains(&inspect()), "{runs:?}");
    assert_eq!(
        runs.last(),
        Some(&rm(NET)),
        "the network is removed last: {runs:?}"
    );
}

#[tokio::test]
async fn a_failed_inspect_disconnects_the_bridge_removes_the_network_and_names_the_step() {
    for answer in [
        failed("template parsing error"),
        ok("\n"),
        ok("10.200.0.2 10.200.0.3\n"),
        ok("fd00::2\n"),
        ok("<no value>\n"),
    ] {
        let reply = answer.clone();
        let runner = docker_with(move |argv| (argv == inspect().as_slice()).then(|| reply.clone()));
        let err = gvisor(&runner)
            .prepare_network(run_id(), &first_block())
            .await
            .expect_err("inspect failed");
        assert!(err.to_string().contains("inspect"), "{answer:?}: {err}");
        let runs = runner.runs();
        assert_eq!(&runs[..3], &[create_net(None), connect(), inspect()]);
        let after = &runs[3..];
        let unjoined = after.iter().position(|a| {
            is(a, &["docker", "network", "disconnect"])
                && a.contains(&NET.to_string())
                && a.contains(&BRIDGE.to_string())
        });
        assert!(unjoined.is_some(), "bridge disconnected: {runs:?}");
        assert_eq!(
            after.last(),
            Some(&rm(NET)),
            "network removed last: {runs:?}"
        );
    }
}

#[tokio::test]
async fn without_a_bridge_prepare_is_none_and_runs_nothing() {
    let runner = docker();
    let got = GvisorDriver::new(runner.clone())
        .prepare_network(run_id(), &first_block())
        .await
        .expect("ok");
    assert_eq!(got, None);
    let got = DockerRuncDriver::new(runner.clone())
        .prepare_network(run_id(), &first_block())
        .await
        .expect("ok");
    assert_eq!(got, None);
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn apple_container_keeps_the_old_behaviour() {
    let runner = docker();
    let apple = AppleContainerDriver::new(runner.clone());
    assert_eq!(
        apple
            .prepare_network(run_id(), &first_block())
            .await
            .expect("ok"),
        None
    );
    apple.release_network(&prepared()).await.expect("ok");
    apple.sweep_networks().await.expect("ok");
    assert!(runner.all_calls().is_empty());
}

/// A driver that implements only the required methods.
struct Minimal;

#[async_trait::async_trait]
impl SandboxDriver for Minimal {
    fn name(&self) -> &'static str {
        "minimal"
    }
    fn is_strong(&self) -> bool {
        true
    }
    async fn create(&self, _: &LaunchSpec) -> kestrel_sandbox::Result<SandboxHandle> {
        Ok(SandboxHandle { id: ID.into() })
    }
    async fn start(&self, _: &SandboxHandle) -> kestrel_sandbox::Result<OutputStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn wait(&self, _: &SandboxHandle) -> kestrel_sandbox::Result<ExitStatus> {
        Ok(ExitStatus { code: 0 })
    }
    async fn stop(&self, _: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        Ok(())
    }
    async fn destroy(&self, _: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_trait_defaults_have_no_run_networks() {
    let d = Minimal;
    assert_eq!(
        d.prepare_network(run_id(), &first_block())
            .await
            .expect("ok"),
        None
    );
    d.release_network(&prepared()).await.expect("ok");
    d.sweep_networks().await.expect("ok");
}

// create with RunNetwork --------------------------------------------------------------------------

fn run_network_spec() -> LaunchSpec {
    let mut spec = spec();
    spec.workspace = None;
    spec.network = NetworkPolicy::RunNetwork {
        network: NET.to_string(),
        gateway: Url::parse("http://10.200.0.2:7070/").expect("url"),
    };
    spec.labels = BTreeMap::from([("kestrel.run".to_string(), RUN.to_string())]);
    spec
}

#[tokio::test]
async fn create_with_a_run_network_attaches_only_that_network() {
    for driver in ["gvisor", "docker-runc"] {
        let runner = docker();
        let created = match driver {
            "gvisor" => gvisor(&runner).create(&run_network_spec()).await,
            _ => {
                DockerRuncDriver::new(runner.clone())
                    .with_bridge(BRIDGE)
                    .create(&run_network_spec())
                    .await
            }
        };
        assert_eq!(created.expect("created").id, ID);
        let runs = runner.runs();
        assert_eq!(
            runs.len(),
            1,
            "{driver}: no shared network is made: {runs:?}"
        );
        let argv = &runs[0];
        assert!(is(argv, &["docker", "create"]), "{argv:?}");
        let networks: Vec<&String> = argv
            .windows(2)
            .filter(|w| w[0] == "--network")
            .map(|w| &w[1])
            .collect();
        assert_eq!(networks, vec![NET], "{driver}: {argv:?}");
        assert!(
            !argv
                .iter()
                .any(|a| a.starts_with("--network=") || a == "--net"),
            "{argv:?}"
        );
        assert!(!argv.contains(&"kestrel-gw".to_string()), "{argv:?}");
    }
}

#[tokio::test]
async fn create_with_a_run_network_works_without_a_bridge_too() {
    // The engine only builds RunNetwork from a prepared network, but the driver must not care.
    let runner = docker();
    GvisorDriver::new(runner.clone())
        .create(&run_network_spec())
        .await
        .expect("created");
    let runs = runner.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(
        runs[0]
            .windows(2)
            .any(|w| w[0] == "--network" && w[1] == NET)
    );
}

#[tokio::test]
async fn gateway_only_with_a_bridge_still_uses_the_shared_network() {
    // `gateway.advertise_url` set: no run networks, the old shared network.
    let runner = docker();
    let mut spec = run_network_spec();
    spec.network = NetworkPolicy::GatewayOnly {
        gateway: Url::parse("http://gw.example:7070/").expect("url"),
    };
    gvisor(&runner).create(&spec).await.expect("created");
    let runs = runner.runs();
    assert_eq!(
        runs[0],
        s(&["docker", "network", "create", "--internal", "kestrel-gw"])
    );
    assert!(
        runs[1]
            .windows(2)
            .any(|w| w[0] == "--network" && w[1] == "kestrel-gw")
    );
}

// release_network ---------------------------------------------------------------------------------

#[tokio::test]
async fn release_disconnects_the_bridge_then_removes_the_network() {
    let runner = docker();
    gvisor(&runner)
        .release_network(&prepared())
        .await
        .expect("released");
    assert_eq!(runner.runs(), vec![disconnect(NET), rm(NET)]);
}

#[tokio::test]
async fn release_twice_is_fine() {
    let runner = docker();
    let driver = gvisor(&runner);
    driver.release_network(&prepared()).await.expect("first");
    driver.release_network(&prepared()).await.expect("second");
}

#[tokio::test]
async fn release_tolerates_a_network_that_is_already_gone() {
    let gone = format!("Error response from daemon: network {NET} not found");
    let runner = docker_with(move |argv| {
        (argv == disconnect(NET).as_slice() || argv == rm(NET).as_slice()).then(|| failed(&gone))
    });
    gvisor(&runner)
        .release_network(&prepared())
        .await
        .expect("not found is fine");
}

#[tokio::test]
async fn release_tolerates_a_bridge_that_is_not_connected_and_still_removes_the_network() {
    let runner = docker_with(|argv| {
        (argv == disconnect(NET).as_slice()).then(|| {
            failed(&format!(
                "Error response from daemon: container {BRIDGE} is not connected to network {NET}"
            ))
        })
    });
    gvisor(&runner)
        .release_network(&prepared())
        .await
        .expect("not connected is fine");
    assert_eq!(runner.runs().last(), Some(&rm(NET)));
}

#[tokio::test]
async fn release_reports_other_failures() {
    let runner = FakeRunner::with(|_| failed("Cannot connect to the Docker daemon"));
    assert!(gvisor(&runner).release_network(&prepared()).await.is_err());
}

// sweep_networks ----------------------------------------------------------------------------------

fn ls(instance: Option<&str>) -> Vec<String> {
    let mut argv = s(&["docker", "network", "ls", "--filter", "label=kestrel.run"]);
    if let Some(id) = instance {
        argv.extend(s(&["--filter", &format!("label=kestrel.instance={id}")]));
    }
    argv.extend(s(&["--format", "{{.Name}}"]));
    argv
}

fn listing(names: &'static str) -> Arc<FakeRunner> {
    docker_with(move |argv| is(argv, &["docker", "network", "ls"]).then(|| ok(names)))
}

#[tokio::test]
async fn sweep_removes_every_labelled_network_of_this_instance() {
    let runner = listing("kestrel-gw-aaaaaaaaaaaa\nkestrel-gw-bbbbbbbbbbbb\n");
    gvisor(&runner)
        .with_instance("1a2b3c4d")
        .sweep_networks()
        .await
        .expect("swept");
    assert_eq!(
        runner.runs(),
        vec![
            ls(Some("1a2b3c4d")),
            disconnect("kestrel-gw-aaaaaaaaaaaa"),
            rm("kestrel-gw-aaaaaaaaaaaa"),
            disconnect("kestrel-gw-bbbbbbbbbbbb"),
            rm("kestrel-gw-bbbbbbbbbbbb"),
        ]
    );
}

#[tokio::test]
async fn sweep_without_an_instance_lists_by_the_run_label_only() {
    let runner = listing("");
    gvisor(&runner).sweep_networks().await.expect("swept");
    assert_eq!(runner.runs(), vec![ls(None)]);
}

#[tokio::test]
async fn sweep_skips_names_docker_would_read_as_options_or_that_are_not_names() {
    let runner = listing("--force\n\n  \nbad name\n-rf\nkestrel-gw-cccccccccccc\nx;rm\n");
    gvisor(&runner)
        .with_instance("1a2b3c4d")
        .sweep_networks()
        .await
        .expect("swept");
    let runs = runner.runs();
    assert_eq!(
        runs,
        vec![
            ls(Some("1a2b3c4d")),
            disconnect("kestrel-gw-cccccccccccc"),
            rm("kestrel-gw-cccccccccccc"),
        ]
    );
}

#[tokio::test]
async fn sweep_tolerates_networks_that_vanish_meanwhile() {
    let runner = docker_with(|argv| {
        if is(argv, &["docker", "network", "ls"]) {
            return Some(ok("kestrel-gw-dddddddddddd\n"));
        }
        is(argv, &["docker", "network"]).then(|| {
            failed("Error response from daemon: network kestrel-gw-dddddddddddd not found")
        })
    });
    gvisor(&runner)
        .with_instance("1a2b3c4d")
        .sweep_networks()
        .await
        .expect("a vanished network is fine");
}
