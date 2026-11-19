//! `Engine::with_workspaces` must never silently do nothing (leftover 3 of #21, moved to #22).
//!
//! Today it applies the workspaces only when the engine's inner state is unshared and otherwise
//! returns the engine unchanged. These tests do not care whether the fix is to return an error
//! or to always apply: they accept either, and reject only "Ok but ignored".

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use kestrel_core::{Engine, Limits, Workspaces};
use kestrel_types::WorkspaceSpec;
use url::Url;

use kestrel_types::RunStatus::{Completed, Running};

fn new_engine(h: &Harness) -> Engine {
    Engine::new(
        h.store.clone(),
        vec![Box::new(h.driver.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("gateway url"),
        Limits {
            max_concurrent_runs: 4,
            poll_interval: POLL,
        },
    )
}

#[tokio::test(start_paused = true)]
async fn with_workspaces_on_a_fresh_engine_applies_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(4);
    let ws = Workspaces::new(
        dir.path().join("workspaces"),
        Arc::new(kestrel_sandbox::ProcessRunner),
    );
    let engine = new_engine(&h)
        .with_workspaces(ws)
        .into_result()
        .expect("fresh engine accepts workspaces");
    h.engine = Arc::new(engine);
    h.driver.script(&["ok:done"], 0);
    h.start();
    let mut spec = spec();
    spec.sandbox.workspace = WorkspaceSpec::EmptyDir;
    let id = h.submit(spec).await;

    until("run completed", || h.is(id, &Completed)).await;

    let launched = h.driver.specs();
    assert_eq!(launched.len(), 1);
    assert_eq!(
        launched[0].workspace,
        Some(dir.path().join("workspaces").join(id.to_string()))
    );
}

/// An engine that has already run (its tasks were aborted on shutdown) and is then configured
/// again: the call must either fail loudly or really apply the workspaces.
#[tokio::test(start_paused = true)]
async fn with_workspaces_after_the_engine_ran_is_an_error_or_really_applies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspaces");
    let mut h = Harness::new(4);
    let _live = h.driver.live();
    h.start();
    let mut first = spec();
    first.sandbox.workspace = WorkspaceSpec::EmptyDir;
    let id = h.submit(first).await;
    until("running", || h.is(id, &Running)).await;
    // Shut the engine down with the run's actor still alive, then take the engine back.
    h.shutdown.cancel();
    h.task.take().unwrap().await.unwrap().unwrap();
    let stand_in = Arc::new(new_engine(&h));
    let old = std::mem::replace(&mut h.engine, stand_in);
    let old = Arc::try_unwrap(old)
        .ok()
        .expect("nothing else holds the engine");

    let ws = Workspaces::new(root.clone(), Arc::new(kestrel_sandbox::ProcessRunner));
    match old.with_workspaces(ws).into_result() {
        Err(msg) => {
            // Loud failure: say what was refused and why.
            assert!(msg.to_lowercase().contains("workspaces"), "{msg}");
        }
        Ok(engine) => {
            // Silent success is only acceptable if the workspaces are really there.
            h.engine = Arc::new(engine);
            h.shutdown = tokio_util::sync::CancellationToken::new();
            let _live = h.driver.live();
            h.start();
            let mut next = spec();
            next.sandbox.workspace = WorkspaceSpec::EmptyDir;
            let id2 = h.submit(next).await;
            until("second run launched", || {
                h.driver.specs().len() >= 2 && h.is(id2, &Running)
            })
            .await;
            let launched = h.driver.specs();
            assert_eq!(
                launched.last().unwrap().workspace,
                Some(root.join(id2.to_string())),
                "with_workspaces returned Ok but the workspaces were ignored"
            );
        }
    }
}
