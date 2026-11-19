//! Engine-level tests for per-run workspaces (#17): the engine prepares each run's workspace
//! through [`Workspaces`] and mounts it with `LaunchSpec.workspace`.
//!
//! Every test runs on a paused clock with the fake driver and adapter from `common`. Git goes
//! through `FakeGit` (below), except `s2_...`, which runs the real `git` against a temp repo.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use common::*;
use kestrel_core::{Engine, Limits, Workspaces};
use kestrel_sandbox::{
    CommandOutput, CommandRunner, ProcessRunner, SpawnedProcess, validate_workspace_path,
};
use kestrel_store::ControlCommand;
use kestrel_types::{AgentSpec, RunId, RunStatus, SuspendReason, WorkspaceSpec};
use tempfile::TempDir;
use url::Url;

use RunStatus::{Completed, Running, Suspended};

// ---------------------------------------------------------------------------------------------
// Fake git

const NOT_A_REPO: &str = "fatal: not a git repository (or any of the parent directories): .git";

/// Records every git argv. `ok == true` behaves like git in a healthy repo (`worktree add`
/// creates the directory); `ok == false` fails every command like git outside a repository.
struct FakeGit {
    ok: bool,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeGit {
    fn healthy() -> Arc<Self> {
        Arc::new(Self {
            ok: true,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn not_a_repo() -> Arc<Self> {
        Arc::new(Self {
            ok: false,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn worktree_adds(&self) -> usize {
        self.calls().iter().filter(|a| is_worktree_add(a)).count()
    }
}

fn is_worktree_add(argv: &[String]) -> bool {
    argv.windows(2).any(|w| w[0] == "worktree" && w[1] == "add")
}

#[async_trait]
impl CommandRunner for FakeGit {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
        if !self.ok {
            return Ok(CommandOutput {
                status: 128,
                stdout: String::new(),
                stderr: NOT_A_REPO.into(),
            });
        }
        if is_worktree_add(argv) {
            // `git worktree add -b <branch> <path> <commit>`: the path is second to last.
            let path = PathBuf::from(&argv[argv.len() - 2]);
            std::fs::create_dir_all(&path)?;
            std::fs::write(path.join(".git"), "gitdir: /elsewhere\n")?;
        }
        Ok(CommandOutput {
            status: 0,
            stdout: ".git\n".into(),
            stderr: String::new(),
        })
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        panic!("workspaces never spawn: {argv:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers

/// A temp dir and the workspaces root inside it (not created yet, the engine creates it).
fn ws_root() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("workspaces");
    (dir, root)
}

/// Replaces the harness engine with one on the same store, driver and adapters, plus
/// `workspaces`. Call before `start`.
fn with_workspaces(h: &mut Harness, workspaces: Workspaces) {
    let engine = Engine::new(
        h.store.clone(),
        vec![Box::new(h.driver.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("gateway url"),
        Limits {
            max_concurrent_runs: 4,
            poll_interval: POLL,
        },
    )
    .with_workspaces(workspaces);
    h.engine = Arc::new(engine);
}

fn spec_ws(workspace: WorkspaceSpec) -> AgentSpec {
    let mut s = spec();
    s.sandbox.workspace = workspace;
    s
}

fn worktree(repo: impl Into<PathBuf>) -> AgentSpec {
    spec_ws(WorkspaceSpec::GitWorktree { repo: repo.into() })
}

/// The workspace each `create` was given, in order.
fn launched_workspaces(h: &Harness) -> Vec<Option<PathBuf>> {
    h.driver.specs().into_iter().map(|s| s.workspace).collect()
}

fn failed(h: &Harness, id: RunId) -> bool {
    h.is_failed(id).is_some()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Kestrel Test",
            "-c",
            "user.email=test@kestrel.invalid",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A real git repo with one commit, in a temp dir.
fn real_repo() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    (dir, repo)
}

// 1 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s1_empty_dir_run_launches_with_its_own_empty_absolute_workspace() {
    let (_tmp, root) = ws_root();
    let mut h = Harness::new(4);
    let git = FakeGit::healthy();
    with_workspaces(&mut h, Workspaces::new(root.clone(), git.clone()));
    h.driver.script(&["ok:done"], 0);
    h.start();
    let id = h.submit(spec_ws(WorkspaceSpec::EmptyDir)).await;

    until("run completed", || h.is(id, &Completed)).await;

    let expected = root.join(id.to_string());
    assert_eq!(launched_workspaces(&h), vec![Some(expected.clone())]);
    assert!(expected.is_absolute());
    assert!(validate_workspace_path(&expected).is_ok());
    assert!(expected.is_dir(), "workspace exists");
    assert_eq!(
        std::fs::read_dir(&expected).unwrap().count(),
        0,
        "workspace is empty"
    );
    assert!(git.calls().is_empty(), "an empty dir needs no git");
}

#[tokio::test(start_paused = true)]
async fn s1_two_empty_dir_runs_get_different_workspaces() {
    let (_tmp, root) = ws_root();
    let mut h = Harness::new(4);
    with_workspaces(&mut h, Workspaces::new(root.clone(), FakeGit::healthy()));
    let _a = h.driver.live();
    let _b = h.driver.live();
    h.start();
    let a = h.submit(spec_ws(WorkspaceSpec::EmptyDir)).await;
    let b = h.submit(spec_ws(WorkspaceSpec::EmptyDir)).await;

    until("both running", || h.is(a, &Running) && h.is(b, &Running)).await;

    let mut got: Vec<PathBuf> = launched_workspaces(&h).into_iter().flatten().collect();
    got.sort();
    let mut want = vec![root.join(a.to_string()), root.join(b.to_string())];
    want.sort();
    assert_eq!(got, want);
}

// 2 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s2_two_git_worktree_runs_on_one_repo_get_separate_worktrees_real_git() {
    let (_repo_dir, repo) = real_repo();
    let (_tmp, root) = ws_root();
    let mut h = Harness::new(4);
    with_workspaces(
        &mut h,
        Workspaces::new(root.clone(), Arc::new(ProcessRunner)),
    );
    let _a = h.driver.live();
    let _b = h.driver.live();
    h.start();
    let a = h.submit(worktree(&repo)).await;
    let b = h.submit(worktree(&repo)).await;

    until("both running", || h.is(a, &Running) && h.is(b, &Running)).await;

    let ws: Vec<PathBuf> = launched_workspaces(&h).into_iter().flatten().collect();
    assert_eq!(ws.len(), 2, "both runs launched with a workspace");
    assert_ne!(ws[0], ws[1]);
    let mut want = vec![root.join(a.to_string()), root.join(b.to_string())];
    want.sort();
    let mut got = ws.clone();
    got.sort();
    assert_eq!(got, want);

    // Each is a checkout of the repo, on its own branch.
    for w in &ws {
        assert!(
            w.join("README.md").is_file(),
            "{} is checked out",
            w.display()
        );
    }
    let branches: Vec<String> = ws
        .iter()
        .map(|w| git(w, &["rev-parse", "--abbrev-ref", "HEAD"]))
        .collect();
    assert_ne!(branches[0], branches[1]);
    assert!(branches.iter().all(|b| b.starts_with("kestrel/run-")));

    // A file written in one is not visible in the other.
    std::fs::write(ws[0].join("only-here.txt"), "a").unwrap();
    assert!(!ws[1].join("only-here.txt").exists());
}

#[tokio::test(start_paused = true)]
async fn s2_two_git_worktree_runs_with_fake_git_add_one_worktree_each() {
    let (_tmp, root) = ws_root();
    let mut h = Harness::new(4);
    let git = FakeGit::healthy();
    with_workspaces(&mut h, Workspaces::new(root.clone(), git.clone()));
    let _a = h.driver.live();
    let _b = h.driver.live();
    h.start();
    let a = h.submit(worktree("/srv/repo")).await;
    let b = h.submit(worktree("/srv/repo")).await;

    until("both running", || h.is(a, &Running) && h.is(b, &Running)).await;

    let ws: Vec<PathBuf> = launched_workspaces(&h).into_iter().flatten().collect();
    assert_eq!(ws.len(), 2);
    assert_ne!(ws[0], ws[1]);
    assert_eq!(git.worktree_adds(), 2);
}

// 3 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s3_repo_that_is_not_a_git_repo_fails_the_run_before_any_sandbox() {
    let (_tmp, root) = ws_root();
    let mut h = Harness::new(4);
    with_workspaces(&mut h, Workspaces::new(root, FakeGit::not_a_repo()));
    h.start();
    let id = h.submit(worktree("/srv/not-a-repo")).await;

    until("run failed", || failed(&h, id)).await;
    settle().await;

    let msg = h.is_failed(id).unwrap();
    assert!(
        msg.contains("not a git repository"),
        "clear failure message, got: {msg}"
    );
    assert!(
        msg.contains("/srv/not-a-repo"),
        "names the repo, got: {msg}"
    );
    assert_eq!(h.driver.count(&Call::Create), 0, "no sandbox was created");
    assert!(h.sandbox_id(id).is_none());
}

#[tokio::test(start_paused = true)]
async fn s3_real_non_repo_dir_fails_the_run_before_any_sandbox_real_git() {
    let plain = tempfile::tempdir().expect("temp dir");
    let (_tmp, root) = ws_root();
    let mut h = Harness::new(4);
    with_workspaces(&mut h, Workspaces::new(root, Arc::new(ProcessRunner)));
    h.start();
    let id = h.submit(worktree(plain.path())).await;

    until("run failed", || failed(&h, id)).await;

    let msg = h.is_failed(id).unwrap();
    assert!(msg.contains("not a git repository"), "got: {msg}");
    assert_eq!(h.driver.count(&Call::Create), 0);
}

// 4 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s4_relative_repo_path_fails_with_a_clear_message_and_runs_no_git() {
    let (_tmp, root) = ws_root();
    let mut h = Harness::new(4);
    let git = FakeGit::healthy();
    with_workspaces(&mut h, Workspaces::new(root, git.clone()));
    h.start();
    let id = h.submit(worktree("some/repo")).await;

    until("run failed", || failed(&h, id)).await;
    settle().await;

    let msg = h.is_failed(id).unwrap();
    assert!(msg.contains("some/repo"), "names the path, got: {msg}");
    assert!(
        msg.contains("absolute"),
        "says it must be absolute, got: {msg}"
    );
    assert!(
        git.calls().is_empty(),
        "the engine does not guess a base dir"
    );
    assert_eq!(h.driver.count(&Call::Create), 0);
}

#[tokio::test(start_paused = true)]
async fn s4_yaml_default_repo_dot_is_relative_and_fails_the_same_way() {
    // `workspace: git-worktree` in YAML means repo `.`; the CLI must resolve it before submit.
    let yaml = "name: tester\nruntime: claude-code\nrole: tester\n\
                model_policy: { default: tier:mid }\n\
                sandbox: { tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: git-worktree }\n\
                budget: { usd: \"1\", wall_clock: 1h }\n";
    let spec = AgentSpec::from_yaml(yaml).expect("spec parses");
    assert_eq!(
        spec.sandbox.workspace,
        WorkspaceSpec::GitWorktree { repo: ".".into() }
    );

    let (_tmp, root) = ws_root();
    let mut h = Harness::new(4);
    let git = FakeGit::healthy();
    with_workspaces(&mut h, Workspaces::new(root, git.clone()));
    h.start();
    let id = h.submit(spec).await;

    until("run failed", || failed(&h, id)).await;
    settle().await;

    let msg = h.is_failed(id).unwrap();
    assert!(msg.contains("absolute"), "got: {msg}");
    assert!(git.calls().is_empty());
    assert_eq!(h.driver.count(&Call::Create), 0);
}

// 5 ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s5_run_resumed_by_a_new_engine_reuses_its_workspace_without_git() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("kestrel.db");
    let root = dir.path().join("workspaces");
    let paused = Suspended(SuspendReason::Manual);

    let (id, first_ws) = {
        let mut first = Harness::on(&db, 4);
        let git = FakeGit::healthy();
        with_workspaces(&mut first, Workspaces::new(root.clone(), git.clone()));
        let _s = first.driver.live();
        first.start();
        let id = first.submit(worktree("/srv/repo")).await;
        until("running", || first.is(id, &Running)).await;
        first.control(id, ControlCommand::Pause);
        until("paused", || first.is(id, &paused)).await;
        assert_eq!(git.worktree_adds(), 1);
        let ws = launched_workspaces(&first);
        assert_eq!(ws, vec![Some(root.join(id.to_string()))]);
        (id, ws[0].clone().unwrap())
    };

    // A new engine on the same database and workspaces root.
    let mut h = Harness::on(&db, 4);
    let git = FakeGit::healthy();
    with_workspaces(&mut h, Workspaces::new(root.clone(), git.clone()));
    let report = h.engine.recover().await.expect("recover");
    assert_eq!(report.suspended, vec![id]);
    let _resumed = h.driver.live();
    h.start();
    h.control(id, ControlCommand::Resume);
    until("running again", || h.is(id, &Running)).await;

    assert_eq!(launched_workspaces(&h), vec![Some(first_ws)]);
    assert_eq!(git.worktree_adds(), 0, "the existing worktree is reused");
}

// 6 ---------------------------------------------------------------------------------------------
//
// `SandboxSpec.workspace` is a required `WorkspaceSpec` with no default: a YAML spec without the
// field does not parse, so "no workspace" can only mean an engine built without `Workspaces`.
// That engine behaves as before #17: no workspace is mounted.

#[test]
fn s6_spec_without_workspace_field_does_not_parse() {
    let yaml = "name: tester\nruntime: claude-code\nrole: tester\n\
                model_policy: { default: tier:mid }\n\
                sandbox: { tier: auto, image: fake-image, cpu: 1, memory: 512Mi }\n\
                budget: { usd: \"1\", wall_clock: 1h }\n";
    assert!(AgentSpec::from_yaml(yaml).is_err());
}

#[tokio::test(start_paused = true)]
async fn s6_engine_without_workspaces_mounts_nothing_as_before() {
    let mut h = Harness::new(4);
    h.driver.script(&["ok:done"], 0);
    h.start();
    let id = h.submit(spec_ws(WorkspaceSpec::EmptyDir)).await;

    until("run completed", || h.is(id, &Completed)).await;
    assert_eq!(launched_workspaces(&h), vec![None]);
}
