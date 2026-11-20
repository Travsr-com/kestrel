//! Issue #10, engine side: the engine asks the driver for the run's network BEFORE it builds the
//! launch plan, uses the returned IP as the gateway host in the plan (scheme, port and path stay
//! as configured), passes the network name to `create`, and cleans up when something fails
//! between `prepare_network` and a created sandbox. `recover` also sweeps leftover networks.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod net_support;

use common::*;
use kestrel_sandbox::NetworkPolicy;
use kestrel_types::RunStatus::{Completed, Running};
use net_support::{harness, network_for};

#[tokio::test(start_paused = true)]
async fn the_engine_prepares_the_network_before_it_creates_the_sandbox() {
    let (mut h, net) = harness(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;

    let log = net.log();
    assert_eq!(log[0], format!("prepare:{id}"), "{log:?}");
    assert_eq!(log[1], "create", "{log:?}");
    assert_eq!(
        net.removed(),
        Vec::<String>::new(),
        "a live run keeps its network"
    );
}

#[tokio::test(start_paused = true)]
async fn the_plan_uses_the_prepared_ip_as_the_gateway_host() {
    let (mut h, _net) = harness(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;

    let ctx = h
        .adapters
        .plans()
        .into_iter()
        .flatten()
        .next()
        .expect("a plan");
    let url = &ctx.gateway_url;
    assert_eq!(url.host_str(), Some("10.77.0.1"), "{url}");
    // Scheme, port and the `/r/<token>` path stay as configured.
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.port(), Some(7070));
    let path: Vec<&str> = url.path_segments().expect("path").collect();
    assert_eq!(path.first(), Some(&"r"), "{url}");
    assert_eq!(path.len(), 2, "{url}");
}

#[tokio::test(start_paused = true)]
async fn create_gets_the_network_through_the_launch_spec() {
    let (mut h, _net) = harness(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;

    let specs = h.driver.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].prepared_network, Some(network_for(id)));
    assert!(matches!(
        specs[0].network,
        NetworkPolicy::GatewayOnly { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn two_runs_get_different_networks_and_their_own_gateway_ip() {
    let (mut h, _net) = harness(4);
    let _a = h.driver.live();
    let _b = h.driver.live();
    h.start();
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;
    until("both running", || h.is(a, &Running) && h.is(b, &Running)).await;

    let specs = h.driver.specs();
    let mut nets: Vec<_> = specs
        .iter()
        .filter_map(|s| s.prepared_network.clone())
        .collect();
    nets.sort();
    nets.dedup();
    assert_eq!(nets.len(), 2, "{specs:?}");
    let mut hosts: Vec<String> = h
        .adapters
        .plans()
        .into_iter()
        .flatten()
        .map(|c| c.gateway_url.host_str().unwrap_or_default().to_string())
        .collect();
    hosts.sort();
    assert_eq!(hosts, vec!["10.77.0.1", "10.77.0.2"]);
}

#[tokio::test(start_paused = true)]
async fn a_failing_prepare_fails_the_run_with_no_create_and_nothing_leaked() {
    let (mut h, net) = harness(4);
    net.fail_prepare("Error response from daemon: boom");
    h.start();
    let id = h.submit(spec()).await;
    until("failed", || h.is_failed(id).is_some()).await;

    let why = h.is_failed(id).expect("failed");
    assert!(why.contains("boom"), "{why}");
    assert!(h.driver.calls().is_empty(), "{:?}", h.driver.calls());
    assert_eq!(net.position("create"), None);
    // The driver cleans up its own half-built network; the engine has nothing to remove.
    assert_eq!(net.removed(), Vec::<String>::new());
    assert!(h.driver.leaked().is_empty());
    assert!(
        h.adapters.plans().into_iter().flatten().next().is_none(),
        "no plan was built for a run without a network"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_create_removes_the_prepared_network() {
    let (mut h, net) = harness(4);
    h.driver.fail_next_create("docker create failed");
    h.start();
    let id = h.submit(spec()).await;
    until("failed", || h.is_failed(id).is_some()).await;

    assert_eq!(net.removed(), vec![network_for(id)]);
    let log = net.log();
    assert!(
        net.position("remove:").unwrap() > net.position("create").unwrap(),
        "{log:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_network_removal_does_not_hide_the_create_error() {
    let (mut h, net) = harness(4);
    net.fail_remove("cannot remove");
    h.driver.fail_next_create("docker create failed");
    h.start();
    let id = h.submit(spec()).await;
    until("failed", || h.is_failed(id).is_some()).await;
    assert!(h.is_failed(id).unwrap().contains("docker create failed"));
}

#[tokio::test(start_paused = true)]
async fn a_finished_run_destroys_its_sandbox_and_the_driver_owns_the_network_cleanup() {
    let (mut h, net) = harness(4);
    h.driver.script(&["ok:done"], 0);
    h.start();
    let id = h.submit(spec()).await;
    until("completed", || h.is(id, &Completed)).await;
    until("destroyed", || net.position("destroy:").is_some()).await;

    // `destroy` takes down the network inside the driver, so the engine does not remove it twice.
    assert_eq!(net.removed(), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn a_driver_without_per_run_networks_keeps_the_configured_gateway() {
    let mut h = Harness::new(4);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &Running)).await;

    assert_eq!(h.driver.specs()[0].prepared_network, None);
    let ctx = h
        .adapters
        .plans()
        .into_iter()
        .flatten()
        .next()
        .expect("a plan");
    assert_eq!(ctx.gateway_url.host_str(), Some("127.0.0.1"));
}

#[tokio::test(start_paused = true)]
async fn recover_removes_leftover_networks() {
    let (h, net) = harness(4);
    net.set_leftover_networks(&["kestrel-gw-aaaaaaaaaaaa", "kestrel-gw-bbbbbbbbbbbb"]);
    h.engine.recover().await.expect("recover");
    assert_eq!(
        net.removed(),
        vec!["kestrel-gw-aaaaaaaaaaaa", "kestrel-gw-bbbbbbbbbbbb"]
    );
}

#[tokio::test(start_paused = true)]
async fn recover_survives_a_failing_network_sweep() {
    let (h, net) = harness(4);
    net.fail_leftover_networks("daemon down");
    h.engine.recover().await.expect("recover still works");
    assert!(net.removed().is_empty());

    let (h, net) = harness(4);
    net.set_leftover_networks(&["kestrel-gw-aaaaaaaaaaaa", "kestrel-gw-bbbbbbbbbbbb"]);
    net.fail_remove("cannot remove");
    h.engine.recover().await.expect("recover still works");
    assert_eq!(
        net.removed().len(),
        2,
        "the second network is tried after the first fails"
    );
}
