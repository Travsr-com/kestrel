//! Issue #10: when the workspace cannot be prepared after the run's network was created, no
//! sandbox will ever exist, so the engine removes the network itself.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod net_support;

use std::path::PathBuf;
use std::sync::Arc;

use common::*;
use kestrel_core::{Engine, Limits, Workspaces};
use kestrel_sandbox::ProcessRunner;
use kestrel_types::{AgentSpec, WorkspaceSpec};
use net_support::{harness, network_for};
use url::Url;

#[tokio::test(start_paused = true)]
async fn a_failing_workspace_removes_the_prepared_network() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let (mut h, net) = harness(4);
    h.engine = Arc::new(
        Engine::new(
            h.store.clone(),
            vec![Box::new(net.clone())],
            adapter_factory(h.adapters.clone()),
            Url::parse(GATEWAY).expect("gateway url"),
            Limits {
                max_concurrent_runs: 4,
                poll_interval: POLL,
            },
        )
        .with_workspaces(Workspaces::new(
            tmp.path().join("workspaces"),
            Arc::new(ProcessRunner),
        ))
        .into_result()
        .expect("workspaces applied"),
    );
    h.start();
    let mut s: AgentSpec = spec();
    // A relative repo path is refused before any git call.
    s.sandbox.workspace = WorkspaceSpec::GitClone {
        repo: PathBuf::from("relative/repo"),
        depth: None,
    };
    let id = h.submit(s).await;
    until("failed", || h.is_failed(id).is_some()).await;

    assert_eq!(net.removed(), vec![network_for(id)]);
    assert_eq!(net.position("create"), None, "no sandbox was created");
}
