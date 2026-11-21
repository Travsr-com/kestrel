//! Issue #43: a run's config directory (the files of its launch plan, such as the MCP config) is
//! mounted read-only at `/kestrel`, outside `/workspace`, so the agent can read it but neither
//! change it nor commit it.

mod common;

use common::{FakeRunner, spec};
use kestrel_sandbox::{AppleContainerDriver, GvisorDriver, SandboxDriver, SandboxError};

fn mounts(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|w| w[0] == "--mount")
        .map(|w| w[1].clone())
        .collect()
}

#[tokio::test]
async fn the_config_dir_is_mounted_read_only_at_kestrel() {
    let dir = tempfile::tempdir().expect("config dir");
    let mut spec = spec();
    spec.config = Some(dir.path().to_path_buf());
    let runner = FakeRunner::ok();
    GvisorDriver::new(runner.clone())
        .create(&spec)
        .await
        .expect("create");
    let want = format!(
        "type=bind,source={},target=/kestrel,readonly",
        dir.path().display()
    );
    let got = mounts(&runner.runs()[0]);
    assert!(got.contains(&want), "{got:?}");
    // The workspace mount is still there, and separate.
    assert!(
        got.iter().any(|m| m.ends_with("target=/workspace")),
        "{got:?}"
    );

    let runner = FakeRunner::ok();
    AppleContainerDriver::new(runner.clone())
        .create(&spec)
        .await
        .expect("create");
    assert!(mounts(&runner.runs()[0]).contains(&want));
}

#[tokio::test]
async fn no_config_dir_means_no_kestrel_mount() {
    let runner = FakeRunner::ok();
    GvisorDriver::new(runner.clone())
        .create(&spec())
        .await
        .expect("create");
    assert!(
        !mounts(&runner.runs()[0])
            .iter()
            .any(|m| m.contains("/kestrel"))
    );
}

#[tokio::test]
async fn a_config_dir_that_would_change_the_mount_options_is_refused() {
    for bad in [
        "relative/dir",
        "/tmp/a,target=/etc",
        "/tmp/a=b",
        "/tmp/a\"b",
    ] {
        let mut spec = spec();
        spec.config = Some(bad.into());
        let runner = FakeRunner::ok();
        let err = GvisorDriver::new(runner.clone())
            .create(&spec)
            .await
            .expect_err(bad);
        assert!(
            matches!(err, SandboxError::InvalidConfig { .. }),
            "{bad}: {err}"
        );
        assert!(runner.runs().is_empty(), "{bad}: the runner was called");
    }
}
