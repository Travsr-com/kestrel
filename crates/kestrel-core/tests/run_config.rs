//! Issue #43, scenario 5: the files of a run's launch plan (its MCP config) are written to a
//! directory of the run's own, outside the workspace, mounted read-only at `/kestrel`, and removed
//! when the run is cleaned up. The root of those directories is its owner's only, so no other host
//! user can read a run's token; the run's own directory is readable by any user, so the sandbox
//! can read it whichever user its image runs as.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Harness, spec, until};
use kestrel_agent::PlanFile;
use kestrel_types::RunId;
use kestrel_types::RunStatus;

fn mcp_file() -> PlanFile {
    PlanFile {
        name: "mcp.json".into(),
        contents: r#"{"mcpServers":{}}"#.into(),
    }
}

#[tokio::test(start_paused = true)]
async fn plan_files_live_in_a_private_dir_of_the_run_until_it_ends() {
    let root = tempfile::tempdir().unwrap();
    let mut h = Harness::new(4);
    let path = root.path().to_path_buf();
    h.rebuild(|e| e.with_run_config(path));
    h.adapters.set_files(vec![mcp_file()]);
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &RunStatus::Running)).await;

    let dir = h.driver.specs()[0].config.clone().expect("a config dir");
    assert_eq!(dir, root.path().join(id.to_string()));
    let file = dir.join("mcp.json");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        r#"{"mcpServers":{}}"#
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(root.path()), 0o700);
        assert_eq!(mode(&dir), 0o755);
        assert_eq!(mode(&file), 0o644);
    }

    s.line("ok:done");
    s.exit(0);
    until("completed", || h.is(id, &RunStatus::Completed)).await;
    until("cleaned up", || !dir.exists()).await;
}

#[tokio::test(start_paused = true)]
async fn a_plan_without_files_gets_no_config_dir() {
    let root = tempfile::tempdir().unwrap();
    let mut h = Harness::new(4);
    let path = root.path().to_path_buf();
    h.rebuild(|e| e.with_run_config(path));
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    assert_eq!(h.driver.specs()[0].config, None);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    s.line("ok:done");
    s.exit(0);
    until("completed", || h.is(id, &RunStatus::Completed)).await;
}

#[tokio::test(start_paused = true)]
async fn a_file_name_that_is_a_path_fails_the_run_instead_of_writing_outside() {
    let root = tempfile::tempdir().unwrap();
    let mut h = Harness::new(4);
    let path = root.path().to_path_buf();
    h.rebuild(|e| e.with_run_config(path));
    h.adapters.set_files(vec![PlanFile {
        name: "../escape.json".into(),
        contents: "{}".into(),
    }]);
    let _s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("failed", || h.is_failed(id).is_some()).await;
    assert!(!root.path().join("escape.json").exists());
    assert!(h.is_failed(id).unwrap_or_default().contains("escape.json"));
}

// A run stopped between its end and its cleanup leaves its config behind; recovery removes it.
#[tokio::test(start_paused = true)]
async fn recovery_removes_the_config_left_by_a_finished_run() {
    let root = tempfile::tempdir().unwrap();
    let mut h = Harness::new(4);
    let path = root.path().to_path_buf();
    h.rebuild(|e| e.with_run_config(path));
    h.driver.script(&["ok:done"], 0);
    h.start();
    let id = h.submit(spec()).await;
    until("completed", || h.is(id, &RunStatus::Completed)).await;
    h.shutdown.cancel();
    if let Some(task) = h.task.take() {
        task.await.unwrap().unwrap();
    }
    let left = root.path().join(id.to_string());
    std::fs::create_dir_all(&left).unwrap();
    std::fs::write(left.join("mcp.json"), "{}").unwrap();
    // A directory that is not a finished run's is left alone.
    let other = root.path().join(RunId::new().to_string());
    std::fs::create_dir_all(&other).unwrap();

    let path = root.path().to_path_buf();
    h.rebuild(|e| e.with_run_config(path));
    h.engine.recover().await.expect("recover");
    assert!(!left.exists());
    assert!(other.exists());
}

// Without a configured root, each engine gets a private one of its own: never a shared,
// predictable temp path another user could make first.
#[tokio::test(start_paused = true)]
async fn the_default_root_is_private_to_the_engine() {
    let mut h = Harness::new(4);
    h.adapters.set_files(vec![mcp_file()]);
    let s = h.driver.live();
    h.start();
    let id = h.submit(spec()).await;
    until("running", || h.is(id, &RunStatus::Running)).await;
    let dir = h.driver.specs()[0].config.clone().expect("a config dir");
    let root = dir.parent().expect("a root").to_path_buf();
    assert_ne!(root, std::env::temp_dir().join("kestrel-run-config"));
    let name = root.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("kestrel-run-config-"), "{name}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    s.line("ok:done");
    s.exit(0);
    until("completed", || h.is(id, &RunStatus::Completed)).await;
    let _ = std::fs::remove_dir_all(&root);
}
