//! Scenario tests for per-run workspaces (#17).
//!
//! Unit tests drive git through `FakeGit`, which records every argv and imitates just enough
//! of git (`worktree add` creates the directory). One test, `real_git_...`, runs the real
//! `git` binary against a temp repo.

// Test helpers fail by panicking.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use kestrel_core::workspace::{WorkspaceError, Workspaces};
use kestrel_sandbox::{
    CommandOutput, CommandRunner, GvisorDriver, LaunchSpec, NetworkPolicy, ProcessRunner,
    SandboxDriver, SandboxError, SpawnedProcess,
};
use kestrel_types::{RunId, WorkspaceSpec};
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// Fake git
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Mode {
    /// Behaves like git in a healthy repo.
    Ok,
    /// Every command fails the way git does outside a repository.
    NotARepo,
    /// The repo check passes, `worktree add` fails (for example the branch already exists).
    WorktreeAddFails,
    /// The runner cannot start git at all (binary missing).
    CannotSpawn,
}

const NOT_A_REPO: &str = "fatal: not a git repository (or any of the parent directories): .git";
const BRANCH_EXISTS: &str = "fatal: a branch named 'kestrel/run-123456789abc' already exists";

struct FakeGit {
    mode: Mode,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeGit {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Calls that are `git ... worktree add ...`.
    fn worktree_adds(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|argv| is_worktree_add(argv))
            .collect()
    }

    fn respond(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        let fail = |status, stderr: &str| {
            Ok(CommandOutput {
                status,
                stdout: String::new(),
                stderr: stderr.to_string(),
            })
        };
        match self.mode {
            Mode::CannotSpawn => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory (os error 2)",
            )),
            Mode::NotARepo => fail(128, NOT_A_REPO),
            Mode::WorktreeAddFails if is_worktree_add(argv) => fail(255, BRANCH_EXISTS),
            Mode::Ok if is_worktree_add(argv) => {
                // `git worktree add -b <branch> <path> <commit>`: the path is the second to last
                // argument. Imitate git: create it with a `.git` file and a checked-out file.
                let path = PathBuf::from(&argv[argv.len() - 2]);
                std::fs::create_dir_all(&path)?;
                std::fs::write(path.join(".git"), "gitdir: /elsewhere\n")?;
                std::fs::write(path.join("README.md"), "hello\n")?;
                Ok(CommandOutput::default())
            }
            // Any repo check (`rev-parse` and friends) succeeds.
            _ => Ok(CommandOutput {
                status: 0,
                stdout: ".git\n".to_string(),
                stderr: String::new(),
            }),
        }
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
        self.respond(argv)
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        // Workspaces only need commands run to completion.
        panic!("unexpected spawn: {argv:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// A run id with a fixed, known tail (`123456789abc`).
fn run_a() -> RunId {
    RunId::from_uuid(Uuid::parse_str("0192f0a1-2b3c-7d4e-8f50-123456789abc").unwrap())
}

/// Same timestamp prefix as `run_a`, different tail (`fedcba987654`).
fn run_b() -> RunId {
    RunId::from_uuid(Uuid::parse_str("0192f0a1-2b3c-7d4e-8f50-fedcba987654").unwrap())
}

/// A temp dir plus the `workspaces` root inside it (not created yet).
fn root() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspaces");
    (tmp, root)
}

fn repo() -> PathBuf {
    PathBuf::from("/srv/repos/app")
}

fn git_spec(repo: &Path) -> WorkspaceSpec {
    WorkspaceSpec::GitClone {
        repo: repo.to_path_buf(),
        depth: None,
    }
}

fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

fn is_empty_dir(p: &Path) -> bool {
    p.is_dir() && std::fs::read_dir(p).unwrap().next().is_none()
}

/// The sandbox's own rules (kestrel-sandbox `check_workspace`), restated so a failure here
/// says which rule broke.
fn assert_mountable(p: &Path) {
    assert!(p.is_absolute(), "not absolute: {}", p.display());
    let text = p.to_str().expect("workspace path is not UTF-8");
    for bad in [',', '=', '"', '\n', '\r'] {
        assert!(!text.contains(bad), "contains {bad:?}: {text}");
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario 1: empty-dir gives a new, empty, absolute directory unique to the run.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s1_empty_dir_is_a_new_empty_absolute_dir_under_root() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root.clone(), git.clone());

    let path = ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap();

    assert!(path.is_absolute(), "{}", path.display());
    assert!(
        path.starts_with(&root),
        "{} not under {}",
        path.display(),
        root.display()
    );
    assert!(is_empty_dir(&path), "not an empty dir: {}", path.display());
    assert!(
        git.calls().is_empty(),
        "empty-dir ran git: {:?}",
        git.calls()
    );
}

#[tokio::test]
async fn s1_empty_dir_is_unique_per_run() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::Ok));

    let a = ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap();
    let b = ws.prepare(run_b(), &WorkspaceSpec::EmptyDir).await.unwrap();

    assert_ne!(a, b);
    std::fs::write(a.join("note.txt"), "a").unwrap();
    assert!(is_empty_dir(&b), "run b sees run a's file");
}

#[tokio::test]
async fn s1_path_for_is_root_slash_full_run_id_and_matches_prepare() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root.clone(), FakeGit::new(Mode::Ok));

    let expected = root.join("0192f0a1-2b3c-7d4e-8f50-123456789abc");
    assert_eq!(ws.path_for(run_a()), expected);
    // path_for is pure: nothing on disk yet.
    assert!(!expected.exists());

    let got = ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn s1_root_is_created_when_missing() {
    let (_tmp, root) = root();
    let nested = root.join("deeper").join("still");
    let ws = Workspaces::new(nested.clone(), FakeGit::new(Mode::Ok));

    let path = ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap();

    assert!(nested.is_dir());
    assert!(is_empty_dir(&path));
}

#[tokio::test]
async fn s1_io_failure_creating_the_dir_is_an_io_error() {
    // The root is a regular file, so no directory can be created under it.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspaces");
    std::fs::write(&root, "not a dir").unwrap();
    let ws = Workspaces::new(root, FakeGit::new(Mode::Ok));

    let err = ws
        .prepare(run_a(), &WorkspaceSpec::EmptyDir)
        .await
        .unwrap_err();

    assert!(matches!(err, WorkspaceError::Io { .. }), "{err:?}");
}

#[test]
fn s1_branch_name_uses_the_run_id_tail() {
    assert_eq!(Workspaces::branch_for(run_a()), "kestrel/run-123456789abc");
    assert_eq!(Workspaces::branch_for(run_b()), "kestrel/run-fedcba987654");
}

// ---------------------------------------------------------------------------------------------
// Scenario 2: git-worktree gives each run its own worktree and branch.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s2_git_worktree_runs_exact_worktree_add() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root.clone(), git.clone());

    let path = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    let expected_path = root.join("0192f0a1-2b3c-7d4e-8f50-123456789abc");
    assert_eq!(path, expected_path);
    let p = expected_path.to_str().unwrap();
    assert_eq!(
        git.worktree_adds(),
        vec![s(&[
            "git",
            "-C",
            "/srv/repos/app",
            "worktree",
            "add",
            "-b",
            "kestrel/run-123456789abc",
            p,
            "HEAD",
        ])]
    );
    assert!(path.is_dir());
}

#[tokio::test]
async fn s2_every_git_call_targets_the_repo_explicitly() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());

    ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    let calls = git.calls();
    assert!(!calls.is_empty());
    for argv in calls {
        assert_eq!(argv[0], "git", "{argv:?}");
        assert_eq!(argv[1..3], s(&["-C", "/srv/repos/app"]), "{argv:?}");
    }
}

#[tokio::test]
async fn s2_two_runs_get_separate_worktrees_and_branches() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());

    let a = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();
    let b = ws.prepare(run_b(), &git_spec(&repo())).await.unwrap();

    assert_ne!(a, b);
    let adds = git.worktree_adds();
    assert_eq!(adds.len(), 2, "{adds:?}");
    let branches: HashSet<&String> = adds.iter().map(|argv| &argv[6]).collect();
    let paths: HashSet<&String> = adds.iter().map(|argv| &argv[7]).collect();
    assert_eq!(branches.len(), 2, "branches shared: {adds:?}");
    assert_eq!(paths.len(), 2, "paths shared: {adds:?}");
}

#[tokio::test]
async fn s2_worktree_add_failure_is_a_git_error_with_stderr() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::WorktreeAddFails);
    let ws = Workspaces::new(root, git);

    let err = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap_err();

    match &err {
        WorkspaceError::Git { status, stderr, .. } => {
            assert_eq!(*status, 255);
            assert!(stderr.contains("already exists"), "{stderr}");
        }
        other => panic!("expected Git, got {other:?}"),
    }
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[tokio::test]
async fn s2_git_not_startable_is_an_io_error() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::CannotSpawn));

    let err = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::Io { .. }), "{err:?}");
}

/// Real git: two runs, two worktrees on two branches, files do not leak between them, and a
/// second `prepare` for the same run reuses the worktree.
#[tokio::test]
async fn real_git_two_runs_get_isolated_worktrees() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIPPED real_git_two_runs_get_isolated_worktrees: `git` is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=Kestrel Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-q", "-m", "init"]);

    let ws = Workspaces::new(tmp.path().join("workspaces"), Arc::new(ProcessRunner));
    let spec = git_spec(&repo);
    let a = ws.prepare(run_a(), &spec).await.unwrap();
    let b = ws.prepare(run_b(), &spec).await.unwrap();

    assert_ne!(a, b);
    assert_eq!(
        std::fs::read_to_string(a.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(b.join("README.md")).unwrap(),
        "hello\n"
    );

    std::fs::write(a.join("only-in-a.txt"), "a").unwrap();
    assert!(!b.join("only-in-a.txt").exists(), "file leaked into run b");
    assert!(
        !repo.join("only-in-a.txt").exists(),
        "file leaked into the source repo"
    );

    let branches = git(&[
        "branch",
        "--list",
        "kestrel/run-*",
        "--format=%(refname:short)",
    ]);
    let mut branches: Vec<&str> = branches.lines().collect();
    branches.sort_unstable();
    assert_eq!(
        branches,
        vec!["kestrel/run-123456789abc", "kestrel/run-fedcba987654"]
    );

    // Restart: same run, same worktree, and the file written before is still there.
    let again = ws.prepare(run_a(), &spec).await.unwrap();
    assert_eq!(again, a);
    assert!(a.join("only-in-a.txt").exists());
}

// ---------------------------------------------------------------------------------------------
// Scenario 3: a repo path that is not a git repo fails clearly, before any worktree exists.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s3_not_a_git_repo_fails_with_clear_message() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::NotARepo);
    let ws = Workspaces::new(root.clone(), git);

    let err = ws
        .prepare(run_a(), &git_spec(Path::new("/srv/not-a-repo")))
        .await
        .unwrap_err();

    match &err {
        WorkspaceError::NotAGitRepo { repo } => assert_eq!(repo, Path::new("/srv/not-a-repo")),
        other => panic!("expected NotAGitRepo, got {other:?}"),
    }
    let msg = err.to_string();
    assert!(msg.contains("/srv/not-a-repo"), "{msg}");
    assert!(msg.contains("not a git repository"), "{msg}");
}

#[tokio::test]
async fn s3_not_a_git_repo_leaves_no_workspace_behind() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root.clone(), FakeGit::new(Mode::NotARepo));

    ws.prepare(run_a(), &git_spec(Path::new("/srv/not-a-repo")))
        .await
        .unwrap_err();

    // A leftover directory would make the next prepare "reuse" a broken workspace.
    assert!(!ws.path_for(run_a()).exists());
}

// ---------------------------------------------------------------------------------------------
// Scenario 4: after a restart, a resumed run reuses its existing workspace.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s4_git_worktree_prepare_twice_reuses_without_second_worktree_add() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let first = Workspaces::new(root.clone(), git.clone());
    let a1 = first.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    // A new `Workspaces` over the same root stands in for the restarted engine.
    let restarted = Workspaces::new(root, git.clone());
    let a2 = restarted
        .prepare(run_a(), &git_spec(&repo()))
        .await
        .unwrap();

    assert_eq!(a1, a2);
    assert_eq!(git.worktree_adds().len(), 1, "{:?}", git.calls());
}

#[tokio::test]
async fn s4_empty_dir_prepare_twice_keeps_existing_files() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::Ok));
    let a1 = ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap();
    std::fs::write(a1.join("progress.txt"), "half done").unwrap();

    let a2 = ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap();

    assert_eq!(a1, a2);
    assert_eq!(
        std::fs::read_to_string(a2.join("progress.txt")).unwrap(),
        "half done"
    );
}

// ---------------------------------------------------------------------------------------------
// Scenario 5: the workspace path passes the sandbox's path validation.
// ---------------------------------------------------------------------------------------------

/// Answers `docker create` with a container id; everything else succeeds silently.
struct FakeDocker;

#[async_trait]
impl CommandRunner for FakeDocker {
    async fn run(&self, _argv: &[String]) -> std::io::Result<CommandOutput> {
        Ok(CommandOutput {
            status: 0,
            stdout: "sandbox-1\n".to_string(),
            stderr: String::new(),
        })
    }
    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        panic!("unexpected spawn: {argv:?}");
    }
}

/// Hands `workspace` to a real sandbox driver; it must not be rejected as InvalidWorkspace.
async fn sandbox_accepts(workspace: PathBuf) {
    let spec = LaunchSpec {
        image: "alpine:3.20".to_string(),
        argv: s(&["true"]),
        env: BTreeMap::new(),
        workspace: Some(workspace),
        cpu_millis: 1000,
        memory_mb: 256,
        network: NetworkPolicy::None,
    };
    match GvisorDriver::new(Arc::new(FakeDocker)).create(&spec).await {
        Err(e @ SandboxError::InvalidWorkspace { .. }) => panic!("sandbox rejected: {e}"),
        other => {
            other.unwrap();
        }
    }
}

#[tokio::test]
async fn s5_empty_dir_path_passes_sandbox_validation() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::Ok));

    let path = ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap();

    assert_mountable(&path);
    sandbox_accepts(path).await;
}

#[tokio::test]
async fn s5_git_worktree_path_passes_sandbox_validation() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::Ok));

    let path = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    assert_mountable(&path);
    sandbox_accepts(path).await;
}

#[tokio::test]
async fn s5_root_with_comma_is_rejected_before_any_git_call() {
    let (tmp, _) = root();
    let bad_root = tmp.path().join("work,source=/");
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(bad_root.clone(), git.clone());

    let err = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::InvalidPath { .. }), "{err:?}");
    assert!(git.calls().is_empty(), "git ran: {:?}", git.calls());
    assert!(!bad_root.exists(), "created a directory at an invalid path");
}

#[tokio::test]
async fn s5_root_with_quote_equals_or_newline_is_rejected() {
    for bad in ["ws\"x", "ws=x", "ws\nx", "ws\rx"] {
        let (tmp, _) = root();
        let bad_root = tmp.path().join(bad);
        let git = FakeGit::new(Mode::Ok);
        let ws = Workspaces::new(bad_root.clone(), git.clone());

        for spec in [WorkspaceSpec::EmptyDir, git_spec(&repo())] {
            let err = ws.prepare(run_a(), &spec).await.unwrap_err();
            assert!(
                matches!(err, WorkspaceError::InvalidPath { .. }),
                "{bad:?}: {err:?}"
            );
        }
        assert!(
            git.calls().is_empty(),
            "{bad:?}: git ran: {:?}",
            git.calls()
        );
        assert!(
            !bad_root.exists(),
            "{bad:?}: created a directory at an invalid path"
        );
    }
}

#[tokio::test]
async fn s5_relative_root_is_rejected() {
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(PathBuf::from("relative/workspaces"), git.clone());

    let err = ws
        .prepare(run_a(), &WorkspaceSpec::EmptyDir)
        .await
        .unwrap_err();

    assert!(matches!(err, WorkspaceError::InvalidPath { .. }), "{err:?}");
    assert!(git.calls().is_empty());
    assert!(!Path::new("relative").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn s5_non_utf8_root_is_rejected() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let (tmp, _) = root();
    let bad_root = tmp.path().join(OsStr::from_bytes(b"ws-\xff"));
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(bad_root.clone(), git.clone());

    let err = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::InvalidPath { .. }), "{err:?}");
    assert!(git.calls().is_empty());
    assert!(!bad_root.exists());
}
