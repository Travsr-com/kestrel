//! Review fixes for run networks (PR #62): the subnet plan leaves out this instance's own leftover
//! run networks, `release_network` is decided by `docker network rm`, and the bridge's address is
//! read with `docker container inspect`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{FakeRunner, s};
use kestrel_sandbox::{
    CommandOutput, GvisorDriver, Ipv4Cidr, PreparedNetwork, SandboxDriver, SandboxError,
    SubnetPool, docker_subnets_except_instance,
};
use kestrel_types::RunId;

const RUN: &str = "01890a5d-ac96-774b-bcce-b302099a8057";
const NET: &str = "kestrel-gw-b302099a8057";
const BRIDGE: &str = "kestrel-gateway-b29b95f5";
const INSTANCE: &str = "1a2b3c4d";

/// Our leftover run network, another instance's run network, and a user's network.
const OURS: &str = "aaaaaaaaaaaa";
const THEIRS: &str = "bbbbbbbbbbbb";
const USERS: &str = "cccccccccccc";

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

fn cidr(text: &str) -> Ipv4Cidr {
    Ipv4Cidr::parse(text).expect("cidr")
}

fn ls_all() -> Vec<String> {
    s(&["docker", "network", "ls", "-q"])
}

fn ls_ours() -> Vec<String> {
    s(&[
        "docker",
        "network",
        "ls",
        "-q",
        "--filter",
        "label=kestrel.run",
        "--filter",
        &format!("label=kestrel.instance={INSTANCE}"),
    ])
}

/// Docker with three networks on the first three blocks of `10.200.0.0/16`, ours on the first.
/// `inspect` prints the subnet of each id it is given, in order.
fn three_networks() -> Arc<FakeRunner> {
    FakeRunner::with(|argv| {
        if argv == ls_all().as_slice() {
            return ok(&format!("{OURS}\n{THEIRS}\n{USERS}\n"));
        }
        if argv == ls_ours().as_slice() {
            return ok(&format!("{OURS}\n"));
        }
        if argv.get(1..3) == Some(&s(&["network", "inspect"])[..]) {
            let subnets: Vec<&str> = argv
                .iter()
                .filter_map(|a| match a.as_str() {
                    OURS => Some("10.200.0.0/29 "),
                    THEIRS => Some("10.200.0.8/29 "),
                    USERS => Some("10.200.0.16/29 "),
                    _ => None,
                })
                .collect();
            return ok(&format!("{}\n", subnets.join("\n")));
        }
        failed("unexpected command")
    })
}

#[tokio::test]
async fn our_own_leftover_run_network_is_not_excluded_and_foreign_networks_are() {
    let runner = three_networks();
    let got = docker_subnets_except_instance(runner.as_ref(), Some(INSTANCE))
        .await
        .expect("subnets");
    assert_eq!(got, vec![cidr("10.200.0.8/29"), cidr("10.200.0.16/29")]);
    let runs = runner.runs();
    assert_eq!(runs.len(), 3, "{runs:?}");
    assert_eq!(runs[0], ls_all());
    assert_eq!(runs[1], ls_ours());
    assert!(
        !runs[2].iter().any(|a| a == OURS),
        "our leftover is not inspected: {:?}",
        runs[2]
    );
}

#[tokio::test]
async fn our_leftover_leaves_its_block_free_in_the_plan() {
    let runner = three_networks();
    let plan = SubnetPool::parse("10.200.0.0/29").expect("one block");
    plan.exclude(
        &docker_subnets_except_instance(runner.as_ref(), Some(INSTANCE))
            .await
            .expect("subnets"),
    );
    assert_eq!(plan.available(), 1, "recover removes our leftover later");
}

#[tokio::test]
async fn without_an_instance_every_network_is_excluded() {
    let runner = three_networks();
    let got = docker_subnets_except_instance(runner.as_ref(), None)
        .await
        .expect("subnets");
    assert_eq!(
        got,
        vec![
            cidr("10.200.0.0/29"),
            cidr("10.200.0.8/29"),
            cidr("10.200.0.16/29")
        ]
    );
    assert_eq!(runner.runs().len(), 2, "no second listing");
}

#[tokio::test]
async fn when_every_network_is_ours_nothing_is_inspected() {
    let runner = FakeRunner::with(|argv| {
        if argv == ls_all().as_slice() || argv == ls_ours().as_slice() {
            return ok(&format!("{OURS}\n"));
        }
        failed("unexpected command")
    });
    let got = docker_subnets_except_instance(runner.as_ref(), Some(INSTANCE))
        .await
        .expect("subnets");
    assert!(got.is_empty());
    assert_eq!(runner.runs().len(), 2);
}

#[tokio::test]
async fn an_instance_id_docker_would_read_as_an_option_is_refused_before_any_command() {
    let runner = FakeRunner::ok();
    let err = docker_subnets_except_instance(runner.as_ref(), Some("--all"))
        .await
        .expect_err("refused");
    assert!(matches!(err, SandboxError::InvalidId { .. }), "{err}");
    assert!(runner.all_calls().is_empty());
}

#[tokio::test]
async fn a_failing_listing_of_our_networks_is_an_error() {
    let runner = FakeRunner::with(|argv| {
        if argv == ls_all().as_slice() {
            return ok(&format!("{OURS}\n"));
        }
        failed("Cannot connect to the Docker daemon")
    });
    let err = docker_subnets_except_instance(runner.as_ref(), Some(INSTANCE))
        .await
        .expect_err("fails");
    assert!(err.to_string().contains("Cannot connect"), "{err}");
}

// taken_subnets lists what docker uses, leaving out this instance's run networks ---------------

#[tokio::test]
async fn taken_subnets_runs_the_plan_listing_with_the_instance_filter() {
    let listed = three_networks();
    let expected = docker_subnets_except_instance(listed.as_ref(), Some(INSTANCE))
        .await
        .expect("subnets");
    let runner = three_networks();
    let driver = GvisorDriver::new(runner.clone())
        .with_bridge(BRIDGE)
        .with_instance(INSTANCE);
    let got = driver.taken_subnets().await.expect("subnets");
    assert_eq!(got, expected);
    assert_eq!(got, vec![cidr("10.200.0.8/29"), cidr("10.200.0.16/29")]);
    assert_eq!(runner.runs(), listed.runs(), "the same docker calls");
    assert_eq!(runner.runs()[1], ls_ours());
}

#[tokio::test]
async fn taken_subnets_without_an_instance_lists_every_network() {
    let runner = three_networks();
    let got = GvisorDriver::new(runner.clone())
        .taken_subnets()
        .await
        .expect("subnets");
    assert_eq!(got.len(), 3);
    assert_eq!(runner.runs().len(), 2, "no instance filter");
}

#[tokio::test]
async fn taken_subnets_defaults_to_nothing() {
    struct Bare;
    #[async_trait::async_trait]
    impl SandboxDriver for Bare {
        fn name(&self) -> &'static str {
            "bare"
        }
        fn is_strong(&self) -> bool {
            true
        }
        async fn create(
            &self,
            _: &kestrel_sandbox::LaunchSpec,
        ) -> kestrel_sandbox::Result<kestrel_sandbox::SandboxHandle> {
            Err(SandboxError::InvalidBridgePort)
        }
        async fn start(
            &self,
            _: &kestrel_sandbox::SandboxHandle,
        ) -> kestrel_sandbox::Result<kestrel_sandbox::OutputStream> {
            Err(SandboxError::InvalidBridgePort)
        }
        async fn wait(
            &self,
            _: &kestrel_sandbox::SandboxHandle,
        ) -> kestrel_sandbox::Result<kestrel_sandbox::ExitStatus> {
            Err(SandboxError::InvalidBridgePort)
        }
        async fn stop(&self, _: &kestrel_sandbox::SandboxHandle) -> kestrel_sandbox::Result<()> {
            Ok(())
        }
        async fn destroy(&self, _: &kestrel_sandbox::SandboxHandle) -> kestrel_sandbox::Result<()> {
            Ok(())
        }
    }
    assert!(Bare.taken_subnets().await.expect("default").is_empty());
}

#[tokio::test]
async fn apple_container_has_no_taken_subnets_and_runs_nothing() {
    let runner = three_networks();
    let got = kestrel_sandbox::AppleContainerDriver::new(runner.clone())
        .taken_subnets()
        .await
        .expect("nothing");
    assert!(got.is_empty());
    assert!(runner.all_calls().is_empty());
}

#[test]
fn exclude_counts_new_blocks_and_in_use_counts_blocks_that_come_back() {
    let pool = SubnetPool::parse("10.200.0.0/27").expect("four blocks");
    let a = pool.reserve().expect("first");
    let _b = pool.reserve().expect("second");
    assert_eq!(pool.in_use(), 2);
    assert_eq!(
        pool.exclude(&[cidr("10.200.0.0/28")]),
        2,
        "both in-use blocks"
    );
    assert_eq!(pool.in_use(), 0, "excluded blocks never come back");
    assert_eq!(
        pool.exclude(&[cidr("10.200.0.0/29")]),
        0,
        "already excluded"
    );
    assert_eq!(
        pool.exclude(&[cidr("10.200.0.16/28"), cidr("10.0.0.0/8")]),
        2
    );
    pool.release(&a);
    assert_eq!(pool.in_use(), 0);
    assert_eq!(pool.available(), 0);
}

// release_network is decided by `docker network rm` ---------------------------------------------

fn prepared() -> PreparedNetwork {
    PreparedNetwork {
        name: NET.to_string(),
        gateway_host: "10.200.0.2".parse().expect("ip"),
        subnet: SubnetPool::parse("10.200.0.0/16")
            .expect("pool")
            .reserve()
            .expect("block"),
    }
}

fn disconnect() -> Vec<String> {
    s(&["docker", "network", "disconnect", "--force", NET, BRIDGE])
}

fn rm() -> Vec<String> {
    s(&["docker", "network", "rm", NET])
}

fn gvisor(runner: &Arc<FakeRunner>) -> GvisorDriver {
    GvisorDriver::new(runner.clone()).with_bridge(BRIDGE)
}

#[tokio::test]
async fn a_failed_disconnect_followed_by_a_successful_rm_is_released() {
    let runner = FakeRunner::with(|argv| {
        if argv == disconnect().as_slice() {
            return failed(&format!(
                "Error response from daemon: No such container: {BRIDGE}"
            ));
        }
        ok("")
    });
    gvisor(&runner)
        .release_network(&prepared())
        .await
        .expect("the network is gone, so it is released");
    assert_eq!(runner.runs(), vec![disconnect(), rm()]);
}

#[tokio::test]
async fn a_failed_rm_is_the_error_even_when_the_disconnect_worked() {
    let runner = FakeRunner::with(|argv| {
        if argv == rm().as_slice() {
            return failed(
                "Error response from daemon: error while removing network: has active endpoints",
            );
        }
        ok("")
    });
    let err = gvisor(&runner)
        .release_network(&prepared())
        .await
        .expect_err("still there");
    assert!(err.to_string().contains("active endpoints"), "{err}");
}

#[tokio::test]
async fn when_both_fail_the_rm_failure_is_reported() {
    let runner = FakeRunner::with(|argv| {
        if argv == rm().as_slice() {
            return failed("rm failed: daemon busy");
        }
        failed("disconnect failed: something else")
    });
    let err = gvisor(&runner)
        .release_network(&prepared())
        .await
        .expect_err("still there");
    assert!(err.to_string().contains("rm failed"), "{err}");
}

// the bridge's address is read with `docker container inspect` ----------------------------------

#[tokio::test]
async fn the_bridge_address_is_read_with_container_inspect() {
    let runner = FakeRunner::with(|argv| {
        if argv.get(1).map(String::as_str) == Some("container") {
            return ok("10.200.0.2\n");
        }
        ok("")
    });
    let run: RunId = RUN.parse().expect("run id");
    let net = gvisor(&runner)
        .prepare_network(run, &prepared().subnet)
        .await
        .expect("prepared")
        .expect("a run network");
    assert_eq!(net.gateway_host.to_string(), "10.200.0.2");
    let format = format!("{{{{(index .NetworkSettings.Networks \"{NET}\").IPAddress}}}}");
    let inspect = s(&[
        "docker",
        "container",
        "inspect",
        "--format",
        &format,
        BRIDGE,
    ]);
    assert!(runner.runs().contains(&inspect), "{:?}", runner.runs());
    assert!(
        !runner
            .runs()
            .iter()
            .any(|a| a.get(1).map(String::as_str) == Some("inspect")),
        "plain `docker inspect` could match a network or image of that name"
    );
}

// a network left behind by an aborted prepare is replaced -------------------------------------

/// Docker's answer when a network of that name exists.
fn exists(name: &str) -> CommandOutput {
    failed(&format!(
        "Error response from daemon: network with name {name} already exists"
    ))
}

fn is_create(argv: &[String]) -> bool {
    argv.get(1..3) == Some(&s(&["network", "create"])[..])
}

/// The first `creates` answers to `network create` say the network exists; everything else
/// works, and the bridge has `10.200.0.2`.
fn existing_network(creates: usize, name: &'static str) -> Arc<FakeRunner> {
    let seen = std::sync::atomic::AtomicUsize::new(0);
    FakeRunner::with(move |argv| {
        if is_create(argv) {
            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return if n < creates { exists(name) } else { ok("") };
        }
        if argv.get(1).map(String::as_str) == Some("container") {
            return ok("10.200.0.2\n");
        }
        ok("")
    })
}

/// The step of each docker call: `create`, `connect`, `disconnect`, `rm` or `inspect`.
fn steps(runner: &FakeRunner) -> Vec<String> {
    runner
        .runs()
        .iter()
        .map(|a| match a.get(1).map(String::as_str) {
            Some("container") => "inspect".to_string(),
            _ => a.get(2).cloned().unwrap_or_default(),
        })
        .collect()
}

#[tokio::test]
async fn a_leftover_network_of_the_same_name_is_removed_and_the_create_runs_again() {
    let runner = existing_network(1, NET);
    let run: RunId = RUN.parse().expect("run id");
    let net = gvisor(&runner)
        .prepare_network(run, &prepared().subnet)
        .await
        .expect("prepared")
        .expect("a run network");
    assert_eq!(net.name, NET);
    assert_eq!(net.gateway_host.to_string(), "10.200.0.2");
    assert_eq!(
        steps(&runner),
        ["create", "disconnect", "rm", "create", "connect", "inspect"]
    );
    let runs = runner.runs();
    assert_eq!(runs[1], disconnect());
    assert_eq!(runs[2], rm());
    assert_eq!(runs[0], runs[3], "the same create, once more");
}

#[tokio::test]
async fn a_second_already_exists_is_an_error_naming_the_create_step() {
    let runner = existing_network(2, NET);
    let run: RunId = RUN.parse().expect("run id");
    let err = gvisor(&runner)
        .prepare_network(run, &prepared().subnet)
        .await
        .expect_err("still there");
    match &err {
        SandboxError::RunNetwork { step, network, .. } => {
            assert_eq!(*step, "create");
            assert_eq!(network, NET);
        }
        other => panic!("expected RunNetwork, got {other:?}"),
    }
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(
        steps(&runner),
        ["create", "disconnect", "rm", "create"],
        "one retry only"
    );
}

#[tokio::test]
async fn already_exists_for_another_name_is_not_retried() {
    let runner = existing_network(1, "kestrel-gw-000000000000");
    let run: RunId = RUN.parse().expect("run id");
    let err = gvisor(&runner)
        .prepare_network(run, &prepared().subnet)
        .await
        .expect_err("not ours to remove");
    assert!(
        matches!(err, SandboxError::RunNetwork { step: "create", .. }),
        "{err}"
    );
    assert_eq!(steps(&runner), ["create"]);
}
