//! Issue #3, scenarios 9 and 10. Need a host with docker and the `runsc` runtime.
//! Run with `cargo test -p kestrel-sandbox --features integration`.

#![cfg(feature = "integration")]

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use kestrel_sandbox::{
    GvisorDriver, LaunchSpec, NetworkPolicy, OutputLine, ProcessRunner, SandboxDriver, StdStream,
};

fn alpine(argv: &[&str]) -> LaunchSpec {
    LaunchSpec {
        image: "alpine:3.20".to_string(),
        argv: argv.iter().map(|a| (*a).to_string()).collect(),
        env: BTreeMap::new(),
        workspace: None,
        cpu_millis: 1000,
        memory_mb: 256,
        network: NetworkPolicy::None,
        labels: BTreeMap::new(),
    }
}

#[tokio::test]
async fn s9_real_gvisor_echo_hi() {
    let driver = GvisorDriver::new(Arc::new(ProcessRunner));

    let h = driver
        .create(&alpine(&["echo", "hi"]))
        .await
        .expect("create");
    let lines: Vec<OutputLine> = driver.start(&h).await.expect("start").collect().await;
    let status = driver.wait(&h).await.expect("wait");
    driver.destroy(&h).await.expect("destroy");

    assert!(
        lines.contains(&OutputLine {
            stream: StdStream::Stdout,
            line: "hi".to_string()
        }),
        "{lines:?}"
    );
    assert_eq!(status.code, 0);
    assert!(status.success());
}

#[tokio::test]
async fn s10_real_gvisor_network_none_blocks_wget() {
    let driver = GvisorDriver::new(Arc::new(ProcessRunner));

    let h = driver
        .create(&alpine(&[
            "wget",
            "-q",
            "-T",
            "5",
            "-O",
            "-",
            "http://example.com",
        ]))
        .await
        .expect("create");
    let lines: Vec<OutputLine> = driver.start(&h).await.expect("start").collect().await;
    let status = driver.wait(&h).await.expect("wait");
    driver.destroy(&h).await.expect("destroy");

    assert_ne!(status.code, 0, "wget must fail without network: {lines:?}");
    assert!(!status.success());
}
