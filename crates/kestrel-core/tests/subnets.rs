//! Issues #34 and #10, engine side: every run gets its own network on a `/29` block from the
//! engine's `SubnetPool` (`Engine::with_subnets`).
//!
//! Contract the tests pin down:
//!
//! * A run reserves a block together with its slot, in the same FIFO line. With no free block the
//!   run stays `Queued` (a resume stays pending) and gets ONE `AgentLog` per wait whose line holds
//!   `subnet pool` and the pool CIDR. A freed block lets the head of the line go.
//! * The actor calls `prepare_network(run, &subnet)` before it builds the launch plan. The run's
//!   gateway URL is the configured one with its host replaced by the prepared `gateway_host`, and
//!   the launch spec uses `NetworkPolicy::RunNetwork`. A driver answering `None` gives the block
//!   back at once and the run is launched as before (`GatewayOnly`).
//! * A taken block (docker's "Pool overlaps") is excluded and another one tried; any other failure
//!   fails the run naming the step and frees the block.
//! * The run's end destroys the sandbox, then `release_network`, then frees the block. A suspended
//!   run keeps both. `shutdown` releases the networks of the sandboxes it destroys and `recover`
//!   calls `sweep_networks`.
//!
//! [`NetDriver`] wraps the shared `FakeDriver` and adds the network methods. Its failures are the
//! errors the real gVisor driver returns for the same docker answers, so the engine is tested
//! against the same error values production sees.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use kestrel_core::{Engine, Limits};
use kestrel_sandbox::{
    CommandOutput, CommandRunner, ExitStatus, GvisorDriver, LaunchSpec, NetworkPolicy,
    OutputStream, PreparedNetwork, SandboxDriver, SandboxError, SandboxHandle, SpawnedProcess,
    Subnet, SubnetPool,
};
use kestrel_store::ControlCommand;
use kestrel_types::{EventKind, RunId, RunStatus, SuspendReason};
use url::Url;

use RunStatus::{Cancelled, Queued, Running, Suspended};

const MANUAL: RunStatus = Suspended(SuspendReason::Manual);
const GRACE: Duration = Duration::from_secs(30);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// NetDriver

/// How the next `prepare_network` fails.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// Docker says the block overlaps a network it already has.
    Taken,
    /// The `docker network connect` of the bridge fails.
    Connect,
}

/// One `release_network` call.
#[derive(Debug, Clone)]
struct Released {
    network: PreparedNetwork,
    /// How many driver calls (`FakeDriver::calls`) came before it.
    after_calls: usize,
    /// Free blocks in the pool while it ran (the block must still be held).
    available: usize,
}

struct NetState {
    inner: FakeDriver,
    /// `false`: the driver has no run networks and answers `None`.
    networks: Mutex<bool>,
    failures: Mutex<VecDeque<Failure>>,
    prepared: Mutex<Vec<(RunId, Subnet)>>,
    released: Mutex<Vec<Released>>,
    sweeps: Mutex<usize>,
    sweep_fails: Mutex<bool>,
    pool: Mutex<Option<Arc<SubnetPool>>>,
}

#[derive(Clone)]
struct NetDriver(Arc<NetState>);

impl NetDriver {
    fn new(inner: FakeDriver) -> NetDriver {
        NetDriver(Arc::new(NetState {
            inner,
            networks: Mutex::new(true),
            failures: Mutex::default(),
            prepared: Mutex::default(),
            released: Mutex::default(),
            sweeps: Mutex::default(),
            sweep_fails: Mutex::default(),
            pool: Mutex::default(),
        }))
    }

    fn without_networks(&self) {
        *lock(&self.0.networks) = false;
    }

    fn fail_next(&self, f: Failure) {
        lock(&self.0.failures).push_back(f);
    }

    fn fail_sweeps(&self) {
        *lock(&self.0.sweep_fails) = true;
    }

    fn watch(&self, pool: Arc<SubnetPool>) {
        *lock(&self.0.pool) = Some(pool);
    }

    /// Every block `prepare_network` was asked for, for `run`, in order.
    fn blocks_of(&self, run: RunId) -> Vec<String> {
        lock(&self.0.prepared)
            .iter()
            .filter(|(r, _)| *r == run)
            .map(|(_, s)| s.cidr().to_string())
            .collect()
    }

    fn prepares(&self) -> usize {
        lock(&self.0.prepared).len()
    }

    fn released(&self) -> Vec<Released> {
        lock(&self.0.released).clone()
    }

    fn sweeps(&self) -> usize {
        *lock(&self.0.sweeps)
    }
}

/// `kestrel-gw-<last 12 hex chars of the run id>`.
fn network_name(run: RunId) -> String {
    let id = run.to_string();
    format!("kestrel-gw-{}", &id[id.len() - 12..])
}

/// The bridge's address on a block: the second host (the first is docker's gateway).
fn bridge_ip(subnet: &Subnet) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(subnet.cidr().addr()) + 2)
}

/// Docker as the gVisor driver sees it when one step fails.
struct Docker(Failure);

#[async_trait]
impl CommandRunner for Docker {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let fail = |stderr: &str| CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: stderr.to_string(),
        };
        Ok(match (self.0, words.get(1..3)) {
            (Failure::Taken, Some(["network", "create"])) => fail(
                "Error response from daemon: Pool overlaps with other one on this address space",
            ),
            (Failure::Connect, Some(["network", "connect"])) => {
                fail("Error response from daemon: No such container: kestrel-gateway-test")
            }
            _ => CommandOutput {
                status: 0,
                stdout: "10.200.0.2\n".to_string(),
                stderr: String::new(),
            },
        })
    }

    async fn spawn(&self, _: &[String]) -> std::io::Result<SpawnedProcess> {
        Err(std::io::Error::other("not used"))
    }
}

/// The error the gVisor driver returns for `failure`.
async fn driver_error(failure: Failure, run: RunId, subnet: &Subnet) -> SandboxError {
    GvisorDriver::new(Arc::new(Docker(failure)))
        .with_bridge("kestrel-gateway-test")
        .prepare_network(run, subnet)
        .await
        .expect_err("the step fails")
}

#[async_trait]
impl SandboxDriver for NetDriver {
    fn name(&self) -> &'static str {
        self.0.inner.name()
    }

    fn is_strong(&self) -> bool {
        true
    }

    async fn create(&self, spec: &LaunchSpec) -> kestrel_sandbox::Result<SandboxHandle> {
        self.0.inner.create(spec).await
    }

    async fn start(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<OutputStream> {
        self.0.inner.start(h).await
    }

    async fn wait(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<ExitStatus> {
        self.0.inner.wait(h).await
    }

    async fn stop(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.0.inner.stop(h).await
    }

    async fn destroy(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.0.inner.destroy(h).await
    }

    async fn prepare_network(
        &self,
        run: RunId,
        subnet: &Subnet,
    ) -> kestrel_sandbox::Result<Option<PreparedNetwork>> {
        lock(&self.0.prepared).push((run, subnet.clone()));
        let failure = lock(&self.0.failures).pop_front();
        if let Some(f) = failure {
            return Err(driver_error(f, run, subnet).await);
        }
        if !*lock(&self.0.networks) {
            return Ok(None);
        }
        Ok(Some(PreparedNetwork {
            name: network_name(run),
            gateway_host: bridge_ip(subnet),
            subnet: subnet.clone(),
        }))
    }

    async fn release_network(&self, network: &PreparedNetwork) -> kestrel_sandbox::Result<()> {
        let available = lock(&self.0.pool)
            .as_ref()
            .map_or(usize::MAX, |p| p.available());
        let after_calls = self.0.inner.calls().len();
        lock(&self.0.released).push(Released {
            network: network.clone(),
            after_calls,
            available,
        });
        Ok(())
    }

    async fn sweep_networks(&self) -> kestrel_sandbox::Result<()> {
        *lock(&self.0.sweeps) += 1;
        if *lock(&self.0.sweep_fails) {
            return Err(SandboxError::CommandFailed {
                program: "docker".into(),
                code: 1,
                stderr: "Cannot connect to the Docker daemon".into(),
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Harness

fn engine(h: &Harness, net: &NetDriver, max: usize) -> Engine {
    Engine::new(
        h.store.clone(),
        vec![Box::new(net.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("gateway url"),
        Limits {
            max_concurrent_runs: max,
            poll_interval: POLL,
        },
    )
}

/// An engine with `max` slots and the pool `pool`, on a fresh store.
fn harness(max: usize, pool: &str) -> (Harness, NetDriver, Arc<SubnetPool>) {
    with(Harness::new(max), max, pool)
}

fn with(mut h: Harness, max: usize, pool: &str) -> (Harness, NetDriver, Arc<SubnetPool>) {
    let net = NetDriver::new(h.driver.clone());
    let pool = Arc::new(SubnetPool::parse(pool).expect("pool"));
    net.watch(pool.clone());
    h.engine = Arc::new(engine(&h, &net, max).with_subnets(pool.clone()));
    (h, net, pool)
}

/// An `AgentLog` saying the run waits for a block of `pool`.
fn waits_for(pool: &str) -> impl Fn(&EventKind) -> bool + '_ {
    move |e| {
        matches!(e, EventKind::AgentLog { line }
            if line.contains("subnet pool") && line.contains(pool))
    }
}

async fn running(h: &Harness, id: RunId) {
    until("running", || h.is(id, &Running)).await;
}

// 1 -------------------------------------------------------------------------------------------
// Scenario 1: a pool of two blocks.

#[tokio::test(start_paused = true)]
async fn s1_with_two_blocks_a_third_run_waits_queued_with_one_note_and_starts_when_one_ends() {
    let (mut h, net, pool) = harness(4, "10.200.0.0/28");
    let sa = h.driver.live();
    let _sb = h.driver.live();
    let _sc = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let b = h.submit(spec()).await;
    running(&h, b).await;
    assert_eq!(pool.available(), 0);

    let c = h.submit(spec()).await;
    until("c is told why it waits", || {
        h.count_events(c, waits_for("10.200.0.0/28")) > 0
    })
    .await;
    settle_for(20).await;
    assert!(h.is(c, &Queued), "c stays queued, it does not fail");
    assert_eq!(
        h.count_events(c, waits_for("10.200.0.0/28")),
        1,
        "one note per wait, not one per tick"
    );
    assert!(net.blocks_of(c).is_empty(), "no network is made for c yet");
    assert_eq!(h.driver.count(&Call::Create), 2);

    sa.line("ok:done");
    sa.exit(0);
    running(&h, c).await;
    assert_eq!(
        net.blocks_of(c),
        net.blocks_of(a),
        "c gets the block a gave back"
    );
    assert_eq!(h.count_events(c, waits_for("10.200.0.0/28")), 1);
    assert_eq!(pool.available(), 0);
}

#[tokio::test(start_paused = true)]
async fn s1_runs_waiting_for_a_block_keep_their_order() {
    let (mut h, _net, _pool) = harness(4, "10.200.0.0/28");
    let sa = h.driver.live();
    let _sb = h.driver.live();
    let sc = h.driver.live();
    let _sd = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let b = h.submit(spec()).await;
    running(&h, b).await;
    let c = h.submit(spec()).await;
    let d = h.submit(spec()).await;
    settle().await;
    assert!(h.is(c, &Queued) && h.is(d, &Queued));

    sa.line("ok:done");
    sa.exit(0);
    running(&h, c).await;
    settle().await;
    assert!(h.is(d, &Queued), "d does not overtake c");
    assert_eq!(h.count_events(d, waits_for("10.200.0.0/28")), 1);

    sc.line("ok:done");
    sc.exit(0);
    running(&h, d).await;
}

#[tokio::test(start_paused = true)]
async fn a_run_cancelled_while_it_waits_for_a_block_never_takes_one() {
    let (mut h, net, pool) = harness(4, "10.200.0.0/29");
    let sa = h.driver.live();
    let _sc = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let b = h.submit(spec()).await;
    until("b waits", || {
        h.count_events(b, waits_for("10.200.0.0/29")) == 1
    })
    .await;
    h.control(b, ControlCommand::Cancel);
    until("b cancelled", || h.is(b, &Cancelled)).await;

    let c = h.submit(spec()).await;
    sa.line("ok:done");
    sa.exit(0);
    running(&h, c).await;
    assert!(net.blocks_of(b).is_empty());
    assert_eq!(pool.available(), 0, "c holds the only block");
}

// 2 -------------------------------------------------------------------------------------------
// The run's network and gateway URL.

#[tokio::test(start_paused = true)]
async fn the_gateway_url_points_at_the_bridge_on_the_runs_own_network() {
    let (mut h, net, _pool) = harness(4, "10.200.0.0/16");
    let _sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;

    assert_eq!(net.blocks_of(a), vec!["10.200.0.0/29"]);
    let token = h.store.run_token(a).expect("token").expect("has token");
    let plans = h.adapters.plans();
    let ctx = &plans[0][0];
    assert_eq!(ctx.gateway_url.scheme(), "http");
    assert_eq!(ctx.gateway_url.host_str(), Some("10.200.0.2"));
    assert_eq!(ctx.gateway_url.port(), Some(7070));
    assert_eq!(ctx.gateway_url.path(), format!("/r/{token}"));

    let specs = h.driver.specs();
    match &specs[0].network {
        NetworkPolicy::RunNetwork { network, gateway } => {
            assert_eq!(network, &network_name(a));
            assert_eq!(gateway.host_str(), Some("10.200.0.2"));
            assert_eq!(gateway.port(), Some(7070));
            assert_eq!(gateway.scheme(), "http");
        }
        other => panic!("expected RunNetwork, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn two_runs_get_two_networks_each_reaching_its_own_bridge_address() {
    let (mut h, net, _pool) = harness(4, "10.200.0.0/16");
    let _sa = h.driver.live();
    let _sb = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    let b = h.submit(spec()).await;
    running(&h, b).await;

    assert_eq!(net.blocks_of(a), vec!["10.200.0.0/29"]);
    assert_eq!(net.blocks_of(b), vec!["10.200.0.8/29"]);
    let hosts: Vec<String> = h
        .adapters
        .plans()
        .iter()
        .map(|p| p[0].gateway_url.host_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(hosts, vec!["10.200.0.2", "10.200.0.10"]);
    let networks: Vec<String> = h
        .driver
        .specs()
        .iter()
        .map(|s| match &s.network {
            NetworkPolicy::RunNetwork { network, .. } => network.clone(),
            other => panic!("expected RunNetwork, got {other:?}"),
        })
        .collect();
    assert_eq!(networks, vec![network_name(a), network_name(b)]);
}

#[tokio::test(start_paused = true)]
async fn the_end_of_a_run_destroys_the_sandbox_then_releases_the_network_then_the_block() {
    let (mut h, net, pool) = harness(4, "10.200.0.0/28");
    h.driver.script(&["ok:done"], 0);
    h.start();
    let a = h.submit(spec()).await;
    until("completed", || h.is(a, &RunStatus::Completed)).await;
    until("network released", || !net.released().is_empty()).await;
    until("block back", || pool.available() == 2).await;

    let released = net.released();
    assert_eq!(released.len(), 1, "released once");
    assert_eq!(released[0].network.name, network_name(a));
    assert_eq!(
        released[0].network.subnet.cidr().to_string(),
        "10.200.0.0/29"
    );
    let sandbox = h.sandbox_id(a).expect("sandbox");
    let destroyed_at = h
        .driver
        .calls()
        .iter()
        .position(|c| *c == Call::Destroy(sandbox.clone()))
        .expect("destroyed");
    assert!(
        destroyed_at < released[0].after_calls,
        "the sandbox is destroyed before its network is released"
    );
    assert_eq!(
        released[0].available, 1,
        "the block is held while its network is released"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_create_still_releases_the_network_and_the_block() {
    let (mut h, net, pool) = harness(4, "10.200.0.0/28");
    h.driver.fail_next_create("no space left on device");
    h.start();
    let a = h.submit(spec()).await;
    until("failed", || h.is_failed(a).is_some()).await;
    until("block back", || pool.available() == 2).await;
    let released = net.released();
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].network.name, network_name(a));
}

#[tokio::test(start_paused = true)]
async fn a_suspended_run_keeps_its_network_and_block() {
    let (mut h, net, pool) = harness(4, "10.200.0.0/28");
    let _first = h.driver.live();
    let again = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    h.control(a, ControlCommand::Pause);
    until("paused", || h.is(a, &MANUAL)).await;
    settle().await;
    assert!(
        net.released().is_empty(),
        "the stopped sandbox still uses it"
    );
    assert_eq!(pool.available(), 1);

    h.control(a, ControlCommand::Resume);
    running(&h, a).await;
    assert_eq!(net.prepares(), 1, "the same sandbox, the same network");
    again.line("ok:done");
    again.exit(0);
    until("released", || net.released().len() == 1).await;
    until("block back", || pool.available() == 2).await;
}

// 3 -------------------------------------------------------------------------------------------
// Failures while preparing the network.

#[tokio::test(start_paused = true)]
async fn a_taken_block_is_skipped_and_another_one_is_tried() {
    let (mut h, net, pool) = harness(4, "10.200.0.0/28");
    net.fail_next(Failure::Taken);
    let sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;

    assert_eq!(net.blocks_of(a), vec!["10.200.0.0/29", "10.200.0.8/29"]);
    let plans = h.adapters.plans();
    assert_eq!(plans[0][0].gateway_url.host_str(), Some("10.200.0.10"));
    assert_eq!(pool.available(), 0);

    sa.line("ok:done");
    sa.exit(0);
    until("completed", || h.is(a, &RunStatus::Completed)).await;
    until("block back", || pool.available() == 1).await;
    settle().await;
    assert_eq!(pool.available(), 1, "the taken block stays out of the pool");
}

#[tokio::test(start_paused = true)]
async fn any_other_prepare_failure_fails_the_run_naming_the_step_and_frees_the_block() {
    let (mut h, net, pool) = harness(4, "10.200.0.0/28");
    net.fail_next(Failure::Connect);
    let _sb = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    until("failed", || h.is_failed(a).is_some()).await;
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(msg.contains("connect"), "{msg}");
    assert_eq!(
        h.driver.count(&Call::Create),
        0,
        "no sandbox without a network"
    );
    until("block back", || pool.available() == 2).await;

    let b = h.submit(spec()).await;
    running(&h, b).await;
    assert_eq!(
        net.blocks_of(b),
        vec!["10.200.0.0/29"],
        "the block is reused"
    );
}

// 4 -------------------------------------------------------------------------------------------
// Drivers without run networks.

#[tokio::test(start_paused = true)]
async fn a_driver_without_run_networks_gives_the_block_back_and_uses_the_shared_network() {
    let (mut h, net, pool) = harness(4, "10.200.0.0/28");
    net.without_networks();
    let _sa = h.driver.live();
    let _sb = h.driver.live();
    let _sc = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;
    let c = h.submit(spec()).await;
    until("all three run on two blocks", || {
        h.is(a, &Running) && h.is(b, &Running) && h.is(c, &Running)
    })
    .await;
    assert_eq!(pool.available(), 2);
    for s in h.driver.specs() {
        match &s.network {
            NetworkPolicy::GatewayOnly { gateway } => {
                assert!(gateway.as_str().starts_with(GATEWAY), "{gateway}");
            }
            other => panic!("expected GatewayOnly, got {other:?}"),
        }
    }
    for p in h.adapters.plans() {
        assert!(p[0].gateway_url.as_str().starts_with(GATEWAY));
    }
    // A run may wait a tick for a block the driver has not given back yet; that is fine.
}

#[tokio::test(start_paused = true)]
async fn without_a_pool_nothing_changes() {
    let mut h = Harness::new(4);
    let net = NetDriver::new(h.driver.clone());
    h.engine = Arc::new(engine(&h, &net, 4));
    let _sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    assert_eq!(net.prepares(), 0);
    assert!(matches!(
        h.driver.specs()[0].network,
        NetworkPolicy::GatewayOnly { .. }
    ));
}

// 5 -------------------------------------------------------------------------------------------
// Shutdown, recovery and resume after a restart.

#[tokio::test(start_paused = true)]
async fn shutdown_releases_the_networks_of_the_sandboxes_it_destroys() {
    let (mut h, net, _pool) = harness(4, "10.200.0.0/28");
    let _sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;

    let report = h.engine.shutdown(GRACE).await;

    assert_eq!(report.destroyed, vec![a]);
    let names: Vec<String> = net
        .released()
        .iter()
        .map(|r| r.network.name.clone())
        .collect();
    assert_eq!(names, vec![network_name(a)]);
}

#[tokio::test(start_paused = true)]
async fn recover_sweeps_leftover_networks_and_a_failing_sweep_is_not_fatal() {
    let (h, net, _pool) = harness(4, "10.200.0.0/28");
    h.engine.recover().await.expect("recover");
    assert_eq!(net.sweeps(), 1);

    let (h, net, _pool) = harness(4, "10.200.0.0/28");
    net.fail_sweeps();
    h.engine.recover().await.expect("best effort");
    assert_eq!(net.sweeps(), 1);
}

#[tokio::test(start_paused = true)]
async fn recover_sweeps_networks_even_without_a_pool() {
    let mut h = Harness::new(4);
    let net = NetDriver::new(h.driver.clone());
    h.engine = Arc::new(engine(&h, &net, 4));
    h.engine.recover().await.expect("recover");
    assert_eq!(net.sweeps(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_resume_after_a_restart_waits_for_a_block_and_then_gets_a_new_network() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("kestrel.db");

    // First engine: `a` runs, then the engine stops.
    let (mut first, _net, _pool) = with(Harness::on(&db, 4), 4, "10.200.0.0/29");
    let _s1 = first.driver.live();
    first.start();
    let a = first.submit(spec()).await;
    running(&first, a).await;
    first.engine.shutdown(GRACE).await;
    drop(first);

    // Second engine, one block: `b` takes it, so the resume of `a` must wait.
    let (mut h, net, _pool) = with(Harness::on(&db, 4), 4, "10.200.0.0/29");
    h.engine.recover().await.expect("recover");
    let restarted = Suspended(SuspendReason::Error("engine restarted".into()));
    assert!(h.is(a, &restarted));
    let sb = h.driver.live();
    let _sa = h.driver.live();
    h.start();
    let b = h.submit(spec()).await;
    running(&h, b).await;

    h.control(a, ControlCommand::Resume);
    until("a is told why it waits", || {
        h.count_events(a, waits_for("10.200.0.0/29")) == 1
    })
    .await;
    settle().await;
    assert!(h.is(a, &restarted), "the resume stays pending");
    assert!(net.blocks_of(a).is_empty());

    sb.line("ok:done");
    sb.exit(0);
    running(&h, a).await;
    assert_eq!(net.blocks_of(a), vec!["10.200.0.0/29"]);
    assert_eq!(h.count_events(a, waits_for("10.200.0.0/29")), 1);
}
