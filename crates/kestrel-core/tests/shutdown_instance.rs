//! Issue #19, instance label: an engine with an instance id puts `kestrel.instance=<id>` on
//! every sandbox next to `kestrel.run`. Without one, behaviour is as before. A driver that
//! panics in `create` fails the run and does not hang `shutdown`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod shutdown_support;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use kestrel_core::{Engine, Limits};
use kestrel_sandbox::{
    ExitStatus, INSTANCE_LABEL, LaunchSpec, OutputStream, RUN_LABEL, SandboxDriver, SandboxHandle,
};
use kestrel_types::RunStatus::Running;
use shutdown_support::Probe;
use url::Url;

fn engine_with(h: &mut Harness, probe: &Probe, instance: Option<&str>) {
    let engine = Engine::new(
        h.store.clone(),
        vec![Box::new(probe.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("gateway url"),
        Limits {
            max_concurrent_runs: 4,
            poll_interval: POLL,
        },
    );
    h.engine = Arc::new(match instance {
        Some(id) => engine.with_instance(id),
        None => engine,
    });
}

#[tokio::test(start_paused = true)]
async fn every_sandbox_carries_the_run_and_the_instance_label() {
    let mut h = Harness::new(4);
    let probe = Probe::new(h.driver.clone());
    engine_with(&mut h, &probe, Some("1a2b3c4d"));
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;

    let specs = h.driver.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].labels.get(RUN_LABEL), Some(&id.to_string()));
    assert_eq!(
        specs[0].labels.get(INSTANCE_LABEL),
        Some(&"1a2b3c4d".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn without_an_instance_only_the_run_label_is_set() {
    let mut h = Harness::new(4);
    let probe = Probe::new(h.driver.clone());
    engine_with(&mut h, &probe, None);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;

    let labels = &h.driver.specs()[0].labels;
    assert_eq!(labels.len(), 1, "{labels:?}");
    assert!(labels.contains_key(RUN_LABEL));
}

#[tokio::test(start_paused = true)]
async fn recover_with_an_instance_destroys_exactly_what_its_driver_lists() {
    let mut h = Harness::new(4);
    let probe = Probe::new(h.driver.clone());
    engine_with(&mut h, &probe, Some("1a2b3c4d"));
    // The driver is limited to this instance, so a sandbox of another instance is not listed.
    probe.set_leftovers(&["mine-1"]);

    h.engine.recover().await.expect("recover");

    assert_eq!(probe.attempts(), vec!["mine-1".to_string()]);
}

/// A driver whose `create` panics.
struct Panics;

#[async_trait]
impl SandboxDriver for Panics {
    fn name(&self) -> &'static str {
        "panics"
    }
    fn is_strong(&self) -> bool {
        true
    }
    async fn create(&self, _: &LaunchSpec) -> kestrel_sandbox::Result<SandboxHandle> {
        panic!("driver bug");
    }
    async fn start(&self, _: &SandboxHandle) -> kestrel_sandbox::Result<OutputStream> {
        unreachable!("never created")
    }
    async fn wait(&self, _: &SandboxHandle) -> kestrel_sandbox::Result<ExitStatus> {
        unreachable!("never created")
    }
    async fn stop(&self, _: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        Ok(())
    }
    async fn destroy(&self, _: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn a_panicking_create_fails_the_run_and_does_not_hang_shutdown() {
    let mut h = Harness::new(4);
    h.engine = Arc::new(Engine::new(
        h.store.clone(),
        vec![Box::new(Panics)],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("gateway url"),
        Limits {
            max_concurrent_runs: 4,
            poll_interval: POLL,
        },
    ));
    h.start();
    let id = h.submit(spec()).await;
    until("failed", || h.is_failed(id).is_some()).await;
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains("sandbox create"), "{msg}");

    let report = tokio::time::timeout(
        Duration::from_secs(60),
        h.engine.shutdown(Duration::from_secs(30)),
    )
    .await
    .expect("shutdown must not wait for a create that died");
    assert!(report.timed_out.is_empty(), "{report:?}");
}
