//! Edges of run networks in the engine: docker refusing every block it is offered, and a network
//! that cannot be removed when its run ends.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use common::*;
use kestrel_core::{Engine, Limits};
use kestrel_sandbox::{
    ExitStatus, LaunchSpec, OutputStream, PreparedNetwork, SandboxDriver, SandboxError,
    SandboxHandle, Subnet, SubnetPool,
};
use kestrel_types::{EventKind, RunId, RunStatus};
use url::Url;

/// What the network methods of [`Edgy`] do.
#[derive(Clone, Copy)]
enum Mode {
    /// Every block is taken by another docker network.
    AlwaysTaken,
    /// Networks are made, but removing one fails.
    ReleaseFails,
}

#[derive(Clone)]
struct Edgy {
    inner: FakeDriver,
    mode: Mode,
    tried: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SandboxDriver for Edgy {
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
        self.tried
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(subnet.cidr().to_string());
        match self.mode {
            Mode::AlwaysTaken => Err(SandboxError::SubnetTaken {
                subnet: subnet.cidr(),
            }),
            Mode::ReleaseFails => Ok(Some(PreparedNetwork {
                name: format!("kestrel-gw-{}", &run.to_string()[24..]),
                gateway_host: "10.200.0.2".parse().expect("ip"),
                subnet: subnet.clone(),
            })),
        }
    }
    async fn release_network(&self, _: &PreparedNetwork) -> kestrel_sandbox::Result<()> {
        Err(SandboxError::CommandFailed {
            program: "docker".into(),
            code: 1,
            stderr: "error while removing network: has active endpoints".into(),
        })
    }
}

fn harness(mode: Mode, pool: &str) -> (Harness, Edgy, Arc<SubnetPool>) {
    let mut h = Harness::new(4);
    let driver = Edgy {
        inner: h.driver.clone(),
        mode,
        tried: Arc::default(),
    };
    let pool = Arc::new(SubnetPool::parse(pool).expect("pool"));
    let engine = Engine::new(
        h.store.clone(),
        vec![Box::new(driver.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("url"),
        Limits {
            max_concurrent_runs: 4,
            poll_interval: POLL,
        },
    )
    .with_subnets(pool.clone());
    h.engine = Arc::new(engine);
    (h, driver, pool)
}

#[tokio::test(start_paused = true)]
async fn when_docker_refuses_every_block_left_the_run_fails_naming_the_pool() {
    let (mut h, driver, pool) = harness(Mode::AlwaysTaken, "10.200.0.0/28");
    h.start();
    let a = h.submit(spec()).await;
    until("failed", || h.is_failed(a).is_some()).await;
    let msg = h.is_failed(a).unwrap_or_default();
    assert!(msg.contains("subnet pool 10.200.0.0/28"), "{msg}");
    assert_eq!(
        *driver.tried.lock().unwrap_or_else(PoisonError::into_inner),
        vec!["10.200.0.0/29", "10.200.0.8/29"]
    );
    assert_eq!(pool.available(), 0, "both blocks are excluded");
    assert_eq!(h.driver.count(&Call::Create), 0);
}

#[tokio::test(start_paused = true)]
async fn the_engine_tries_every_free_block_once_before_it_gives_up() {
    let (mut h, driver, pool) = harness(Mode::AlwaysTaken, "10.200.0.0/25");
    h.start();
    let a = h.submit(spec()).await;
    until("failed", || h.is_failed(a).is_some()).await;
    let tried = driver
        .tried
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    assert_eq!(tried, 16);
    assert_eq!(pool.available(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_network_that_cannot_be_removed_keeps_its_block_out_of_the_pool_and_says_so() {
    let (mut h, _driver, pool) = harness(Mode::ReleaseFails, "10.200.0.0/28");
    h.driver.script(&["ok:done"], 0);
    h.start();
    let a = h.submit(spec()).await;
    until("completed", || h.is(a, &RunStatus::Completed)).await;
    until("logged", || {
        h.has_event(
            a,
            |e| matches!(e, EventKind::AgentLog { line } if line.contains("run network")),
        )
    })
    .await;
    settle().await;
    assert_eq!(pool.available(), 1, "the block may still be in use");
}
