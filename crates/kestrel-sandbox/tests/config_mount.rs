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

/// The value after `flag` in `argv`.
fn after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .map(String::as_str)
}

// The config files are readable by their owner only, so a run without a workspace runs as the
// owner of its config directory, or a non-root agent image could not read them.
#[tokio::test]
async fn without_a_workspace_the_sandbox_runs_as_the_config_dir_owner() {
    let dir = tempfile::tempdir().expect("config dir");
    let mut spec = spec();
    spec.workspace = None;
    spec.config = Some(dir.path().to_path_buf());
    let runner = FakeRunner::ok();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    GvisorDriver::new(runner.clone())
        .with_workspace_owner(move |p| {
            log.lock().expect("lock").push(p.to_path_buf());
            Ok((1234, 5678))
        })
        .create(&spec)
        .await
        .expect("create");
    assert_eq!(after(&runner.runs()[0], "--user"), Some("1234:5678"));
    assert_eq!(*seen.lock().expect("lock"), [dir.path().to_path_buf()]);
}

#[tokio::test]
async fn a_config_dir_whose_owner_cannot_be_read_is_refused_before_any_command() {
    let dir = tempfile::tempdir().expect("config dir");
    let mut spec = spec();
    spec.workspace = None;
    spec.config = Some(dir.path().to_path_buf());
    let runner = FakeRunner::ok();
    let err = GvisorDriver::new(runner.clone())
        .with_workspace_owner(|_| Err(std::io::Error::other("gone")))
        .create(&spec)
        .await
        .expect_err("unreadable config dir");
    assert!(
        matches!(err, SandboxError::ConfigUnreadable { .. }),
        "{err}"
    );
    assert!(runner.runs().is_empty());
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
