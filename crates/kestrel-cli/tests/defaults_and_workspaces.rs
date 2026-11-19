//! Paths the scenario files do not reach: per-run workspaces under `serve`, the git-worktree
//! repo resolved at submit time, default config and data paths, and real driver detection.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{ChildGuard, DEADLINE, Fixture, GOOD_YAML, path_str, poll};
use kestrel_types::{RunId, RunStatus, WorkspaceSpec};
use predicates::prelude::*;

const CONFIG: &str = r#"
[gateway]
listen = "127.0.0.1:0"

[providers.anthropic]
base_url = "http://127.0.0.1:9"
api_key = "test-anthropic-key"
"#;

fn run_id(stdout: &[u8]) -> RunId {
    String::from_utf8_lossy(stdout)
        .trim()
        .parse()
        .expect("run id")
}

#[test]
fn serve_gives_an_empty_dir_run_a_workspace_next_to_the_db() {
    let f = Fixture::new();
    let agent = f.write(
        "empty.yaml",
        &GOOD_YAML.replace("workspace: git-clone", "workspace: empty-dir"),
    );
    let out = f
        .kestrel()
        .args(["run", path_str(&agent), "--input", "x"])
        .assert()
        .success();
    let id = run_id(&out.get_output().stdout);

    let config = f.write("config.toml", CONFIG);
    let mut cmd = f.kestrel_std();
    cmd.args([
        "serve",
        "--config",
        path_str(&config),
        "--sandbox-driver",
        "fake",
    ]);
    let mut serve = ChildGuard::spawn(cmd);
    let store = f.store();
    poll("the run to complete", || {
        let run = store.get_run(id).expect("get").expect("run");
        match run.status {
            RunStatus::Completed => Some(()),
            RunStatus::Failed(e) => panic!("run failed: {e}"),
            _ => None,
        }
    });
    let ws = f.dir.path().join("workspaces").join(id.to_string());
    assert!(ws.is_dir(), "no workspace at {}", ws.display());

    serve.interrupt();
    let status = serve.wait_exit("serve to stop");
    assert!(status.success(), "{status:?}: {}", serve.stderr());
}

#[test]
fn run_resolves_the_worktree_repo_against_the_current_dir() {
    let f = Fixture::new();
    let agent = f.good_agent();
    let out = f
        .kestrel()
        .current_dir(f.dir.path())
        .args(["run", path_str(&agent), "--input", "x"])
        .assert()
        .success();
    let id = run_id(&out.get_output().stdout);
    let spec = f.store().run_spec(id).expect("spec").expect("stored");
    let WorkspaceSpec::GitClone { repo, .. } = spec.sandbox.workspace else {
        panic!("not a git clone: {:?}", spec.sandbox.workspace)
    };
    assert!(repo.is_absolute(), "{}", repo.display());
    assert_eq!(
        repo.canonicalize().expect("repo exists"),
        f.dir.path().canonicalize().expect("dir")
    );
}

#[test]
fn run_watch_json_prints_the_id_then_events() {
    let f = Fixture::new();
    let agent = f.good_agent();
    let mut cmd = f.kestrel_std();
    cmd.args(["run", path_str(&agent), "--input", "x", "--watch", "--json"]);
    let mut run = ChildGuard::spawn(cmd);
    let mut lines = run.stdout_lines();
    let first = common::json_line(&lines.next_line("id line"));
    let id: RunId = first["run_id"]
        .as_str()
        .expect("run_id")
        .parse()
        .expect("id");
    lines.until("RunCreated", |l| l.contains("run_created"));
    let store = f.store();
    common::walk(&store, id, RunStatus::Queued, &[RunStatus::Cancelled]);
    assert!(run.wait_exit("run --watch to exit").success());
}

#[cfg(target_os = "linux")]
#[test]
fn serve_reads_the_default_config_path() {
    let f = Fixture::new();
    let config_home = f.dir.path().join("config");
    f.kestrel()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["serve", "--sandbox-driver", "fake"])
        .timeout(DEADLINE)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(path_str(
            &config_home.join("kestrel").join("config.toml"),
        )));
}

#[cfg(target_os = "linux")]
#[test]
fn serve_falls_back_to_home_dot_config() {
    let f = Fixture::new();
    f.kestrel()
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", f.dir.path())
        .args(["serve", "--sandbox-driver", "fake"])
        .timeout(DEADLINE)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(".config/kestrel/config.toml"));
}

#[cfg(unix)]
#[test]
fn no_home_and_no_db_is_a_runtime_error() {
    assert_cmd::Command::cargo_bin("kestrel")
        .expect("kestrel binary")
        .env_remove("HOME")
        .env_remove("XDG_DATA_HOME")
        .args(["ps"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("HOME"));
}

/// Without the test flag, serve probes the host for a real sandbox. Either none is found (exit 1
/// with install instructions) or one is, and serve runs until SIGINT. Both are fine here.
#[test]
fn serve_without_the_fake_driver_detects_real_sandboxes() {
    let f = Fixture::new();
    let config = f.write("config.toml", CONFIG);
    let mut cmd = f.kestrel_std();
    cmd.args(["serve", "--config", path_str(&config), "--json"]);
    let mut serve = ChildGuard::spawn(cmd);
    let mut lines = serve.stdout_lines();
    let outcome = poll("serve to exit or report ready", || {
        if let Some(status) = serve.child.try_wait().expect("try_wait") {
            return Some(Err(status));
        }
        lines.drain().into_iter().next().map(Ok)
    });
    match outcome {
        Err(status) => {
            assert_eq!(status.code(), Some(1));
            assert!(serve.stderr().contains("sandbox"));
        }
        Ok(line) => {
            assert!(common::json_line(&line)["gateway"].is_string());
            serve.interrupt();
            assert!(serve.wait_exit("serve to stop").success());
        }
    }
}
