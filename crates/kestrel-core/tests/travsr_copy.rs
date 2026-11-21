//! Issue #44 (security rework): the host's private copy of a run's code, for Travsr.
//!
//! Travsr never reads a run's workspace, which the agent may rewrite. Kestrel reads the commit a
//! fresh workspace was cloned at (`Workspaces::head`, before its sandbox exists) and makes its own
//! copy of the source repo at that commit (`clone_at`), with the same git invocation as the
//! workspace clone: hooks off and a scrubbed environment.
//!
//! The fake-git tests check the exact commands; the last test runs the real `git` and skips when
//! it is not installed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use kestrel_core::workspace::{WorkspaceError, Workspaces, clone_at, git_env_scrub};
use kestrel_sandbox::{CommandOutput, CommandRunner, EnvScrub, ProcessRunner, SpawnedProcess};
use kestrel_types::RunId;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// What the fake answers every command with, and every (argv, env) it got.
struct FakeGit {
    status: i32,
    stdout: String,
    /// `clone` makes its target directory first (then fails, when `status` says so).
    calls: Mutex<Vec<(Vec<String>, EnvScrub)>>,
}

impl FakeGit {
    fn new(status: i32, stdout: &str) -> Arc<Self> {
        Arc::new(Self {
            status,
            stdout: stdout.to_string(),
            calls: Mutex::default(),
        })
    }

    fn calls(&self) -> Vec<(Vec<String>, EnvScrub)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn argvs(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().map(|(argv, _)| argv).collect()
    }
}

#[async_trait]
impl CommandRunner for FakeGit {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        self.run_with_env(argv, &EnvScrub::default()).await
    }

    async fn run_with_env(
        &self,
        argv: &[String],
        env: &EnvScrub,
    ) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((argv.to_vec(), env.clone()));
        if argv.get(1).is_some_and(|a| a == "clone") {
            std::fs::create_dir_all(Path::new(argv.last().unwrap()).join(".git"))?;
        }
        Ok(CommandOutput {
            status: self.status,
            stdout: self.stdout.clone(),
            stderr: if self.status == 0 {
                String::new()
            } else {
                "fatal: no".into()
            },
        })
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        panic!("never spawned: {argv:?}");
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

fn text(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

#[tokio::test]
async fn head_reads_the_commit_of_the_run_s_workspace_with_hooks_off_and_a_clean_env() {
    let git = FakeGit::new(0, &format!("{SHA}\n"));
    let ws = Workspaces::new(PathBuf::from("/srv/kestrel/workspaces"), git.clone());
    let run = RunId::new();
    assert_eq!(ws.head(run).await.unwrap(), SHA);
    let path = text(&ws.path_for(run));
    let calls = git.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0,
        strings(&[
            "git",
            "-C",
            &path,
            "-c",
            "core.hooksPath=/dev/null",
            "rev-parse",
            "--verify",
            "HEAD^{commit}",
        ])
    );
    assert_eq!(calls[0].1, git_env_scrub());
}

#[tokio::test]
async fn head_accepts_only_a_commit_id() {
    for out in [
        "",
        "main\n",
        "0123\n",
        "--upload-pack=/bin/sh\n",
        &"g".repeat(40),
    ] {
        let git = FakeGit::new(0, out);
        let ws = Workspaces::new(PathBuf::from("/w"), git);
        let err = ws.head(RunId::new()).await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::BadCommit { .. }),
            "{out:?}: {err}"
        );
    }
    // A sha-256 repository's ids are 64 characters.
    let long = "a".repeat(64);
    let ws = Workspaces::new(PathBuf::from("/w"), FakeGit::new(0, &long));
    assert_eq!(ws.head(RunId::new()).await.unwrap(), long);
    // git's own failure is a git error.
    let ws = Workspaces::new(PathBuf::from("/w"), FakeGit::new(128, ""));
    let err = ws.head(RunId::new()).await.unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Git { status: 128, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn clone_at_copies_the_source_and_checks_out_the_commit_detached() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("copy");
    let repo = dir.path().join("repo");
    let git = FakeGit::new(0, "");
    clone_at(git.as_ref(), &repo, SHA, &dest).await.unwrap();
    let (repo, dest) = (text(&repo), text(&dest));
    assert_eq!(
        git.argvs(),
        [
            strings(&[
                "git",
                "clone",
                "--local",
                "--no-hardlinks",
                "--no-checkout",
                "--config",
                "core.symlinks=false",
                "--",
                &repo,
                &dest,
            ]),
            strings(&[
                "git",
                "-C",
                &dest,
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.symlinks=false",
                "checkout",
                "--detach",
                SHA,
            ]),
        ]
    );
    for (_, env) in git.calls() {
        assert_eq!(env, git_env_scrub());
    }
}

#[tokio::test]
async fn clone_at_refuses_a_bad_commit_or_a_relative_repo_before_any_git_call() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("copy");
    let git = FakeGit::new(0, "");
    let err = clone_at(git.as_ref(), dir.path(), "--detach", &dest)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::BadCommit { .. }), "{err}");
    let err = clone_at(git.as_ref(), Path::new("repo"), SHA, &dest)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::RelativeRepo { .. }), "{err}");
    assert!(git.calls().is_empty());
    assert!(!dest.exists());
}

#[tokio::test]
async fn a_failed_clone_at_leaves_no_half_made_copy() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("copy");
    let git = FakeGit::new(1, "");
    let err = clone_at(git.as_ref(), dir.path(), SHA, &dest)
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Git { status: 1, .. }),
        "{err}"
    );
    assert!(!dest.exists());
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// Runs git in `dir` and returns its trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(repo: &Path, text: &str) {
    std::fs::write(repo.join("lib.rs"), text).unwrap();
    git(repo, &["add", "lib.rs"]);
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            text,
        ],
    );
}

#[tokio::test]
async fn the_private_copy_is_at_the_workspace_s_commit_even_after_the_source_moves_on() {
    if !git_available() {
        eprintln!("SKIPPED: `git` is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    commit(&repo, "pub fn first() {}\n");
    let first = git(&repo, &["rev-parse", "HEAD"]);
    // The run's workspace, cloned the way `Workspaces::prepare` does.
    let ws = Workspaces::new(dir.path().join("workspaces"), Arc::new(ProcessRunner));
    let run = RunId::new();
    let workspace = ws.path_for(run);
    std::fs::create_dir_all(dir.path().join("workspaces")).unwrap();
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--local",
            "--no-hardlinks",
            "--",
            repo.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
    );
    let head = ws.head(run).await.unwrap();
    assert_eq!(head, first);
    // The source moves on, and the agent edits its workspace: neither reaches the copy.
    commit(&repo, "pub fn second() {}\n");
    std::fs::write(workspace.join("lib.rs"), "pub fn agent() {}\n").unwrap();
    let copy = dir.path().join("travsr").join("repo");
    clone_at(&ProcessRunner, &repo, &head, &copy).await.unwrap();
    assert_eq!(git(&copy, &["rev-parse", "HEAD"]), first);
    assert_eq!(
        std::fs::read_to_string(copy.join("lib.rs")).unwrap(),
        "pub fn first() {}\n"
    );
}

/// Commits a link at `name` in `repo` pointing at `target`, the way git stores one (mode
/// 120000, the target as the blob), so no link has to be made on disk.
fn commit_link(repo: &Path, name: &str, target: &Path) {
    let blob = repo.parent().unwrap().join(format!("{name}.target"));
    std::fs::write(&blob, target.to_str().unwrap()).unwrap();
    let sha = git(
        repo,
        &["hash-object", "-w", "--no-filters", blob.to_str().unwrap()],
    );
    git(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{sha},{name}"),
        ],
    );
}

/// Travsr's `init` rewrites `<repo>/.gitignore` and writes `<repo>/.travsrignore` when there is
/// none, following links. So the copy never holds a link from the commit: each is checked out as
/// a plain file holding its target's path, and the copy's config keeps it so for later git calls.
#[tokio::test]
async fn committed_links_are_plain_files_in_the_copy_so_nothing_is_written_through_them() {
    if !git_available() {
        eprintln!("SKIPPED: `git` is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    commit(&repo, "pub fn first() {}\n");
    // A host file and directory the links point at, and a path that does not exist.
    let host = dir.path().join("host");
    std::fs::create_dir_all(&host).unwrap();
    let gitignore = host.join("gitignore");
    std::fs::write(&gitignore, "host\n").unwrap();
    let dangling = host.join("travsrignore");
    let links = [
        (".gitignore", gitignore.clone()),
        (".travsrignore", dangling.clone()),
        (".travsr", host.clone()),
    ];
    for (name, target) in &links {
        commit_link(&repo, name, target);
    }
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "links",
        ],
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let copy = dir.path().join("copy");
    clone_at(&ProcessRunner, &repo, &head, &copy).await.unwrap();
    for (name, target) in &links {
        let meta = std::fs::symlink_metadata(copy.join(name)).unwrap();
        assert!(meta.is_file(), "{name} is not a plain file: {meta:?}");
        assert_eq!(
            std::fs::read_to_string(copy.join(name)).unwrap(),
            target.to_str().unwrap()
        );
    }
    // Written as `travsr init` writes them: the host's files are untouched.
    std::fs::write(copy.join(".gitignore"), "copy\n").unwrap();
    std::fs::write(copy.join(".travsrignore"), "copy\n").unwrap();
    assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "host\n");
    assert!(!dangling.exists());
    let mut left: Vec<_> = std::fs::read_dir(&host)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    left.sort();
    assert_eq!(left, ["gitignore"]);
    // Later git calls in the copy check out links as plain files too.
    let values = git(&copy, &["config", "--get-all", "core.symlinks"]);
    assert!(
        !values.is_empty() && values.lines().all(|v| v == "false"),
        "{values}"
    );
}
