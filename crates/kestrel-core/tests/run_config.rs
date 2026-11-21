//! Issue #43, scenario 5: the files of a run's launch plan (its MCP config) are written to a
//! directory of the run's own, outside the workspace, readable by the owner only, mounted
//! read-only at `/kestrel`, and removed when the run is cleaned up.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Harness, spec, until};
use kestrel_agent::PlanFile;
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
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&dir), 0o700);
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
