//! Review fixes for run networks on the engine side (PR #62):
//!
//! * Taken blocks ("Pool overlaps") are skipped until the pool has none left, not a fixed number
//!   of times, and only then does the run fail, saying why and what to set.
//! * A listed network over a block another of our runs holds still excludes every other block it
//!   covers; the held block is excluded once its run gives it back, and is not counted as held.
//! * A run that already has a network (for example after `run_forever` was stopped and started
//!   again without `recover`) reuses it instead of making a second one.
//! * A command that is not a resume does not give back the block reserved for a pending resume.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::{BTreeSet, VecDeque};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use kestrel_core::{Engine, Limits};
use kestrel_sandbox::{
    ExitStatus, Ipv4Cidr, LaunchSpec, NetworkPolicy, OutputStream, PreparedNetwork, SandboxDriver,
    SandboxError, SandboxHandle, Subnet, SubnetPool,
};
use kestrel_store::ControlCommand;
use kestrel_types::{MicroUsd, RunId, RunStatus, SuspendReason};
use url::Url;

use RunStatus::{Running, Suspended};

const MANUAL: RunStatus = Suspended(SuspendReason::Manual);
const GRACE: Duration = Duration::from_secs(30);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `kestrel-gw-<last 12 hex chars of the run id>`.
fn network_name(run: RunId) -> String {
    let id = run.to_string();
    format!("kestrel-gw-{}", &id[id.len() - 12..])
}

/// One `prepare_network` call: the run, the block, and whether it made a network.
type Prepared = (RunId, String, bool);

#[derive(Default)]
struct NetState {
    /// Consumed one per `prepare_network`: `true` answers that the block is taken.
    taken: Mutex<VecDeque<bool>>,
    prepared: Mutex<Vec<Prepared>>,
    released: Mutex<Vec<String>>,
    /// Networks that exist, by name. A second `network create` with the same name fails, as in
    /// docker.
    live: Mutex<BTreeSet<String>>,
    /// What `taken_subnets` answers: networks made outside Kestrel, or an error.
    outside: Mutex<Option<Result<Vec<Ipv4Cidr>, String>>>,
    /// `taken_subnets` calls.
    listings: Mutex<usize>,
}

/// The shared `FakeDriver` plus run networks that behave like docker's.
#[derive(Clone)]
struct NetDriver {
    inner: FakeDriver,
    state: Arc<NetState>,
}

impl NetDriver {
    fn new(inner: FakeDriver) -> NetDriver {
        NetDriver {
            inner,
            state: Arc::default(),
        }
    }

    fn taken_next(&self, n: usize) {
        lock(&self.state.taken).extend(std::iter::repeat_n(true, n));
    }

    fn prepared(&self) -> Vec<Prepared> {
        lock(&self.state.prepared).clone()
    }

    fn released(&self) -> Vec<String> {
        lock(&self.state.released).clone()
    }

    /// `taken_subnets` reports these networks made outside Kestrel.
    fn outside(&self, cidrs: &[&str]) {
        let cidrs = cidrs.iter().map(|c| Ipv4Cidr::parse(c).expect("cidr"));
        *lock(&self.state.outside) = Some(Ok(cidrs.collect()));
    }

    /// `taken_subnets` fails.
    fn listing_fails(&self) {
        *lock(&self.state.outside) = Some(Err("Cannot connect to the Docker daemon".into()));
    }

    fn listings(&self) -> usize {
        *lock(&self.state.listings)
    }
}

#[async_trait]
impl SandboxDriver for NetDriver {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn is_strong(&self) -> bool {
        true
    }

    async fn create(&self, spec: &LaunchSpec) -> kestrel_sandbox::Result<SandboxHandle> {
        self.inner.create(spec).await
    }

    async fn start(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<OutputStream> {
        self.inner.start(h).await
    }

    async fn wait(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<ExitStatus> {
        self.inner.wait(h).await
    }

    async fn stop(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.inner.stop(h).await
    }

    async fn destroy(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.inner.destroy(h).await
    }

    async fn prepare_network(
        &self,
        run: RunId,
        subnet: &Subnet,
    ) -> kestrel_sandbox::Result<Option<PreparedNetwork>> {
        let cidr = subnet.cidr().to_string();
        if lock(&self.state.taken).pop_front().unwrap_or(false) {
            lock(&self.state.prepared).push((run, cidr, false));
            return Err(SandboxError::SubnetTaken {
                subnet: subnet.cidr(),
            });
        }
        let name = network_name(run);
        let made = lock(&self.state.live).insert(name.clone());
        lock(&self.state.prepared).push((run, cidr, made));
        if !made {
            return Err(SandboxError::RunNetwork {
                step: "create",
                network: name.clone(),
                stderr: format!(
                    "Error response from daemon: network with name {name} already exists"
                ),
            });
        }
        Ok(Some(PreparedNetwork {
            name,
            gateway_host: Ipv4Addr::from(u32::from(subnet.cidr().addr()) + 2),
            subnet: subnet.clone(),
        }))
    }

    async fn release_network(&self, network: &PreparedNetwork) -> kestrel_sandbox::Result<()> {
        lock(&self.state.live).remove(&network.name);
        lock(&self.state.released).push(network.name.clone());
        Ok(())
    }

    async fn taken_subnets(&self) -> kestrel_sandbox::Result<Vec<Ipv4Cidr>> {
        *lock(&self.state.listings) += 1;
        match lock(&self.state.outside).clone() {
            None => Ok(Vec::new()),
            Some(Ok(cidrs)) => Ok(cidrs),
            Some(Err(stderr)) => Err(SandboxError::CommandFailed {
                program: "docker".into(),
                code: 1,
                stderr,
            }),
        }
    }
}

fn with(mut h: Harness, pool: &str) -> (Harness, NetDriver, Arc<SubnetPool>) {
    let net = NetDriver::new(h.driver.clone());
    let pool = Arc::new(SubnetPool::parse(pool).expect("pool"));
    let engine = Engine::new(
        h.store.clone(),
        vec![Box::new(net.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("gateway url"),
        Limits {
            max_concurrent_runs: 4,
            poll_interval: POLL,
        },
    )
    .with_subnets(pool.clone());
    h.engine = Arc::new(engine);
    (h, net, pool)
}

fn harness(pool: &str) -> (Harness, NetDriver, Arc<SubnetPool>) {
    with(Harness::new(4), pool)
}

async fn running(h: &Harness, id: RunId) {
    until("running", || h.is(id, &Running)).await;
}

// 2: taken blocks are skipped while the pool has free ones ------------------------------------

#[tokio::test(start_paused = true)]
async fn eight_taken_blocks_in_a_row_do_not_fail_a_run_while_blocks_remain() {
    let (mut h, net, pool) = harness("10.200.0.0/24");
    net.taken_next(8);
    let _sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    until("running or failed", || {
        h.is(a, &Running) || h.is_failed(a).is_some()
    })
    .await;
    assert!(h.is(a, &Running), "failed: {:?}", h.is_failed(a));
    let tried: Vec<String> = net.prepared().into_iter().map(|(_, b, _)| b).collect();
    assert_eq!(tried.len(), 9, "{tried:?}");
    assert_eq!(tried[8], "10.200.0.64/29", "the 9th block");
    assert_eq!(pool.available(), 32 - 9, "8 excluded, 1 in use");
    let plans = h.adapters.plans();
    assert_eq!(plans[0][0].gateway_url.host_str(), Some("10.200.0.66"));
}

#[tokio::test(start_paused = true)]
async fn when_every_block_is_taken_the_run_fails_saying_why_and_what_to_set() {
    let (mut h, net, pool) = harness("10.200.0.0/27");
    net.taken_next(4);
    h.start();
    let a = h.submit(spec()).await;
    until("failed", || h.is_failed(a).is_some()).await;
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(msg.contains("subnet pool 10.200.0.0/27"), "{msg}");
    assert!(msg.contains("outside Kestrel"), "{msg}");
    assert!(msg.contains("gateway.subnet_pool"), "{msg}");
    assert_eq!(net.prepared().len(), 4, "every block was tried once");
    assert_eq!(pool.available(), 0);
    assert_eq!(h.driver.count(&Call::Create), 0);
}

// 1 (round 2): the message says who holds the rest of the pool ---------------------------------

#[tokio::test(start_paused = true)]
async fn a_taken_block_with_the_rest_held_by_our_own_runs_says_to_start_again_later() {
    let (mut h, net, pool) = harness("10.200.0.0/28");
    net.taken_next(1);
    let _sa = h.driver.live();
    let _sb = h.driver.live();
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;
    h.start();
    until("a settled", || {
        h.is(a, &Running) || h.is_failed(a).is_some()
    })
    .await;
    running(&h, b).await;
    let msg = h.is_failed(a).expect("a has no block left");
    assert!(msg.contains("subnet pool 10.200.0.0/28"), "{msg}");
    assert!(
        msg.contains("this run found 1 of the pool's blocks taken"),
        "{msg}"
    );
    assert!(msg.contains("own runs hold the rest"), "{msg}");
    assert!(msg.contains("once one of them ends"), "{msg}");
    assert!(!msg.contains("gateway.subnet_pool"), "{msg}");
    assert_eq!(pool.in_use(), 1, "b holds the other block");
}

#[tokio::test(start_paused = true)]
async fn taken_blocks_with_none_held_by_our_runs_say_to_set_the_pool() {
    let (mut h, net, _pool) = harness("10.200.0.0/28");
    net.taken_next(2);
    h.start();
    let a = h.submit(spec()).await;
    until("failed", || h.is_failed(a).is_some()).await;
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(msg.contains("subnet pool 10.200.0.0/28"), "{msg}");
    assert!(
        msg.contains("this run found 2 of the pool's blocks taken"),
        "{msg}"
    );
    assert!(msg.contains("outside Kestrel"), "{msg}");
    assert!(
        msg.contains("Set gateway.subnet_pool to a free range"),
        "{msg}"
    );
    assert!(!msg.contains("own runs"), "{msg}");
}

// 3 (round 2): one taken block costs one listing, not one docker call per block ---------------

#[tokio::test(start_paused = true)]
async fn a_listing_that_covers_the_rest_of_the_pool_fails_the_run_without_trying_each_block() {
    let (mut h, net, pool) = harness("10.200.0.0/24");
    net.taken_next(1);
    // Networks outside Kestrel on everything past the first block.
    net.outside(&[
        "10.200.0.8/29",
        "10.200.0.16/28",
        "10.200.0.32/27",
        "10.200.0.64/26",
        "10.200.0.128/25",
    ]);
    h.start();
    let a = h.submit(spec()).await;
    until("failed", || h.is_failed(a).is_some()).await;
    assert_eq!(
        net.prepared().len(),
        1,
        "no attempt per block: {:?}",
        net.prepared()
    );
    assert_eq!(net.listings(), 1);
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(
        msg.contains("this run found 32 of the pool's blocks taken"),
        "{msg}"
    );
    assert!(
        msg.contains("Set gateway.subnet_pool to a free range"),
        "{msg}"
    );
    assert_eq!(pool.available(), 0);
}

#[tokio::test(start_paused = true)]
async fn after_one_taken_block_exactly_one_more_attempt_is_made_when_one_block_is_left() {
    let (mut h, net, _pool) = harness("10.200.0.0/24");
    net.taken_next(2);
    // Everything but the first two blocks is taken outside Kestrel.
    net.outside(&[
        "10.200.0.16/28",
        "10.200.0.32/27",
        "10.200.0.64/26",
        "10.200.0.128/25",
    ]);
    h.start();
    let a = h.submit(spec()).await;
    until("failed", || h.is_failed(a).is_some()).await;
    let tried: Vec<String> = net.prepared().into_iter().map(|(_, b, _)| b).collect();
    assert_eq!(
        tried,
        ["10.200.0.0/29", "10.200.0.8/29"],
        "one more attempt"
    );
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(
        msg.contains("this run found 32 of the pool's blocks taken"),
        "{msg}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_listing_excludes_only_the_refused_block() {
    let (mut h, net, pool) = harness("10.200.0.0/28");
    net.taken_next(1);
    net.listing_fails();
    let _sa = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    until("running or failed", || {
        h.is(a, &Running) || h.is_failed(a).is_some()
    })
    .await;
    assert!(h.is(a, &Running), "failed: {:?}", h.is_failed(a));
    let tried: Vec<String> = net.prepared().into_iter().map(|(_, b, _)| b).collect();
    assert_eq!(tried, ["10.200.0.0/29", "10.200.0.8/29"]);
    assert_eq!(net.listings(), 1);
    assert_eq!(pool.available(), 0, "one excluded, one in use");
}

#[tokio::test(start_paused = true)]
async fn a_listed_block_held_by_another_of_our_runs_is_not_excluded() {
    let (mut h, net, pool) = harness("10.200.0.0/28");
    net.taken_next(1);
    // Docker without an instance filter lists our own run's network too.
    net.outside(&["10.200.0.8/29"]);
    let _sa = h.driver.live();
    let _sb = h.driver.live();
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;
    h.start();
    until("a settled", || {
        h.is(a, &Running) || h.is_failed(a).is_some()
    })
    .await;
    running(&h, b).await;
    let msg = h.is_failed(a).expect("a has no block left");
    assert!(msg.contains("own runs hold the rest"), "{msg}");
    h.control(b, ControlCommand::Cancel);
    until("b cancelled", || h.is(b, &RunStatus::Cancelled)).await;
    settle().await;
    assert_eq!(pool.available(), 1, "b's block came back to the pool");
}

// 1 (round 3): a foreign network over a block another of our runs holds still excludes the rest --

/// The blocks run `run` tried, in order.
fn tried_by(net: &NetDriver, run: RunId) -> Vec<String> {
    net.prepared()
        .into_iter()
        .filter(|(r, _, _)| *r == run)
        .map(|(_, b, _)| b)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_foreign_network_over_the_whole_pool_is_one_listing_even_while_another_run_holds_a_block()
{
    let (mut h, net, pool) = harness("10.200.0.0/24");
    let _sb = h.driver.live();
    h.start();
    let b = h.submit(spec()).await;
    running(&h, b).await;
    net.outside(&["10.200.0.0/24"]);
    net.taken_next(31);
    let a = h.submit(spec()).await;
    until("a failed", || h.is_failed(a).is_some()).await;
    assert_eq!(tried_by(&net, a), ["10.200.0.8/29"], "one attempt");
    assert_eq!(net.listings(), 1);
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(
        msg.contains("this run found 32 of the pool's blocks taken"),
        "{msg}"
    );
    assert!(
        msg.contains("Set gateway.subnet_pool to a free range"),
        "{msg}"
    );
    assert!(!msg.contains("own runs"), "{msg}");
    assert_eq!(pool.available(), 0);
    assert!(h.is(b, &Running), "b keeps its block and its network");

    h.control(b, ControlCommand::Cancel);
    until("b cancelled", || h.is(b, &RunStatus::Cancelled)).await;
    settle().await;
    assert_eq!(
        pool.available(),
        0,
        "b's block lies under the foreign network, so it is excluded once b ends"
    );
}

#[tokio::test(start_paused = true)]
async fn a_foreign_network_over_the_whole_pool_with_no_other_run_is_one_listing() {
    let (mut h, net, _pool) = harness("10.200.0.0/24");
    h.start();
    net.outside(&["10.200.0.0/24"]);
    net.taken_next(31);
    let a = h.submit(spec()).await;
    until("a failed", || h.is_failed(a).is_some()).await;
    assert_eq!(tried_by(&net, a), ["10.200.0.0/29"], "one attempt");
    assert_eq!(net.listings(), 1);
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(
        msg.contains("this run found 32 of the pool's blocks taken"),
        "{msg}"
    );
    assert!(
        msg.contains("Set gateway.subnet_pool to a free range"),
        "{msg}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_foreign_network_over_part_of_the_pool_leaves_a_free_block_outside_it_to_the_run() {
    // Blocks .0 .8 .16 .24. b holds .0; a foreign network covers .0 and .8.
    let (mut h, net, pool) = harness("10.200.0.0/27");
    let _sb = h.driver.live();
    let _sa = h.driver.live();
    h.start();
    let b = h.submit(spec()).await;
    running(&h, b).await;
    net.outside(&["10.200.0.0/28"]);
    net.taken_next(1);
    let a = h.submit(spec()).await;
    until("a settled", || {
        h.is(a, &Running) || h.is_failed(a).is_some()
    })
    .await;
    assert!(h.is(a, &Running), "failed: {:?}", h.is_failed(a));
    assert_eq!(tried_by(&net, a), ["10.200.0.8/29", "10.200.0.16/29"]);
    assert_eq!(net.listings(), 1);
    assert_eq!(pool.available(), 1, ".24 is the only free block");

    h.control(b, ControlCommand::Cancel);
    until("b cancelled", || h.is(b, &RunStatus::Cancelled)).await;
    settle().await;
    assert_eq!(
        pool.available(),
        1,
        "b's block lies under the foreign network, so it does not come back"
    );
}

#[tokio::test(start_paused = true)]
async fn a_foreign_network_over_one_runs_block_counts_only_the_blocks_that_come_back_as_held() {
    // Blocks .0 .8 .16 .24. b holds .0; .8 is refused, so c holds .16. A foreign network then
    // covers .16 and .24: only b's block outside it can come back to the pool.
    let (mut h, net, pool) = harness("10.200.0.0/27");
    let _sb = h.driver.live();
    let _sc = h.driver.live();
    h.start();
    let b = h.submit(spec()).await;
    running(&h, b).await;
    net.taken_next(1);
    let c = h.submit(spec()).await;
    running(&h, c).await;
    assert_eq!(tried_by(&net, c), ["10.200.0.8/29", "10.200.0.16/29"]);
    net.outside(&["10.200.0.16/28"]);
    net.taken_next(1);
    let a = h.submit(spec()).await;
    until("a failed", || h.is_failed(a).is_some()).await;
    assert_eq!(tried_by(&net, a), ["10.200.0.24/29"]);
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(
        msg.contains("this run found 2 of the pool's blocks taken"),
        "{msg}"
    );
    assert!(
        msg.contains("own runs hold the rest (1 in use)"),
        "only b's block comes back: {msg}"
    );
    assert!(!msg.contains("gateway.subnet_pool"), "{msg}");

    h.control(b, ControlCommand::Cancel);
    until("b cancelled", || h.is(b, &RunStatus::Cancelled)).await;
    settle().await;
    assert_eq!(
        pool.available(),
        1,
        "b's block is outside: back in the pool"
    );
    h.control(c, ControlCommand::Cancel);
    until("c cancelled", || h.is(c, &RunStatus::Cancelled)).await;
    settle().await;
    assert_eq!(
        pool.available(),
        1,
        "c's block is under the foreign network"
    );
}

// 3: a run that has a network keeps it across a run_forever restart ----------------------------

#[tokio::test(start_paused = true)]
async fn a_resume_after_run_forever_restarts_without_recover_reuses_the_runs_network() {
    let (mut h, net, pool) = harness("10.200.0.0/28");
    let _first = h.driver.live();
    let _again = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    running(&h, a).await;
    h.control(a, ControlCommand::Pause);
    until("paused", || h.is(a, &MANUAL)).await;
    settle().await;
    assert_eq!(pool.available(), 1);

    // Stop run_forever through its token and start it again, without `recover`.
    h.shutdown.cancel();
    let task = h.task.take().expect("started");
    task.await.expect("join").expect("run_forever ok");
    assert_eq!(pool.available(), 1, "the paused run keeps its block");
    h.shutdown = tokio_util::sync::CancellationToken::new();
    h.start();

    h.control(a, ControlCommand::Resume);
    until("running or failed", || {
        h.is(a, &Running) || h.is_failed(a).is_some()
    })
    .await;
    assert!(h.is(a, &Running), "failed: {:?}", h.is_failed(a));
    assert_eq!(
        net.prepared().len(),
        1,
        "one network for the run, made once"
    );
    assert!(net.released().is_empty(), "the network was never removed");
    assert_eq!(pool.available(), 1, "the block was never freed");
    let specs = h.driver.specs();
    assert_eq!(specs.len(), 2, "a fresh sandbox for the new actor");
    for s in &specs {
        match &s.network {
            NetworkPolicy::RunNetwork { network, gateway } => {
                assert_eq!(network, &network_name(a));
                assert_eq!(gateway.host_str(), Some("10.200.0.2"));
            }
            other => panic!("expected RunNetwork, got {other:?}"),
        }
    }
}

// 5: a command that is not a resume keeps the block reserved for a pending resume --------------

/// Takes the pool's block the moment it is free, once the pool was seen exhausted. Plays a run
/// that the scheduler would hand a freed block to on another thread.
struct Thief {
    stop: Arc<AtomicBool>,
    stole: Arc<Mutex<Option<Subnet>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Thief {
    fn new(pool: Arc<SubnetPool>) -> Thief {
        let stop = Arc::new(AtomicBool::new(false));
        let stole = Arc::new(Mutex::new(None));
        let (s, got) = (stop.clone(), stole.clone());
        let thread = std::thread::spawn(move || {
            while pool.available() != 0 {
                if s.load(Ordering::Relaxed) {
                    return;
                }
                std::hint::spin_loop();
            }
            while !s.load(Ordering::Relaxed) {
                if let Some(block) = pool.reserve() {
                    *lock(&got) = Some(block);
                    return;
                }
                std::hint::spin_loop();
            }
        });
        Thief {
            stop,
            stole,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> Option<Subnet> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().expect("thief thread");
        }
        lock(&self.stole).clone()
    }
}

/// A test that panics before `finish` (in `until`, say) still stops the spinning thread.
impl Drop for Thief {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // Already unwinding, or the thread's panic was reported by `finish`: ignore it here.
            let _ = t.join();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn set_budget_and_resume_in_one_tick_after_a_restart_keep_the_reserved_block() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("kestrel.db");

    // First engine: `a` runs, then the engine stops.
    let (mut first, _net, _pool) = with(Harness::on(&db, 4), "10.200.0.0/29");
    let _s1 = first.driver.live();
    first.start();
    let a = first.submit(spec()).await;
    running(&first, a).await;
    first.engine.shutdown(GRACE).await;
    drop(first);

    // Second engine, one block: the resume of `a` reserves it.
    let (mut h, _net, pool) = with(Harness::on(&db, 4), "10.200.0.0/29");
    h.engine.recover().await.expect("recover");
    let restarted = Suspended(SuspendReason::Error("engine restarted".into()));
    assert!(h.is(a, &restarted));
    let _sa = h.driver.live();
    let thief = Thief::new(pool.clone());
    h.start();
    h.control(a, ControlCommand::SetBudget(MicroUsd(2_000_000)));
    h.control(a, ControlCommand::Resume);
    until("running or failed", || {
        h.is(a, &Running) || h.is_failed(a).is_some()
    })
    .await;
    let stolen = thief.finish();
    assert!(h.is(a, &Running), "failed: {:?}", h.is_failed(a));
    assert!(
        stolen.is_none(),
        "the reserved block was given back: {stolen:?}"
    );
    assert_eq!(
        h.store
            .run_spec(a)
            .expect("spec")
            .expect("stored")
            .budget
            .limit,
        MicroUsd(2_000_000)
    );
}
