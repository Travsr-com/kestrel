//! Scenario tests for per-run workspaces (#17, reworked for per-run git clones in #22).
//!
//! Unit tests drive git through `FakeGit`, which records every argv and imitates just enough
//! of git (`clone` creates the directory with a `.git` entry). The real `git` binary runs in
//! `git_clones.rs`.

// Test helpers fail by panicking.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use kestrel_core::workspace::{WorkspaceError, Workspaces};
use kestrel_sandbox::{
    CommandOutput, CommandRunner, GvisorDriver, LaunchSpec, NetworkPolicy, SandboxDriver,
    SandboxError, SpawnedProcess,
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
    /// The repo check passes; `clone` leaves a half-made directory behind, then fails.
    CloneFails,
    /// The repo check and `clone` pass; the `checkout -b` inside the clone fails.
    CheckoutFails,
    /// The runner cannot start git at all (binary missing).
    CannotSpawn,
}

const NOT_A_REPO: &str = "fatal: not a git repository (or any of the parent directories): .git";
const CLONE_FAILED: &str = "fatal: could not read from remote repository";
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

    /// Calls that are `git clone ...`.
    fn clones(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|argv| is_clone(argv))
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
            Mode::CloneFails if is_clone(argv) => {
                make_clone_dir(argv)?;
                fail(128, CLONE_FAILED)
            }
            Mode::CheckoutFails if is_checkout(argv) => fail(128, BRANCH_EXISTS),
            Mode::Ok | Mode::CheckoutFails if is_clone(argv) => {
                make_clone_dir(argv)?;
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

fn is_clone(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|a| a == "clone")
}

fn is_checkout(argv: &[String]) -> bool {
    argv.iter().any(|a| a == "checkout")
}

/// `git clone ... <source> <path>`: the path is the last argument. Imitate git: create it with
/// a `.git` directory holding `HEAD`, and a checked-out file.
fn make_clone_dir(argv: &[String]) -> std::io::Result<()> {
    let path = PathBuf::from(argv.last().expect("clone has a target"));
    std::fs::create_dir_all(path.join(".git"))?;
    std::fs::write(path.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;
    std::fs::write(path.join("README.md"), "hello\n")
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
// Scenario 2: git-clone gives each run its own clone and branch (fake git, exact argv).
// ---------------------------------------------------------------------------------------------

/// `git clone --local --no-hardlinks <extra> -- <repo> <path>` (`extra` goes before the `--`), then the branch checkout inside the fresh clone with host hooks disabled.
fn expected_calls(
    repo: &str,
    source: &str,
    extra: &[&str],
    path: &Path,
    branch: &str,
) -> Vec<Vec<String>> {
    let p = path.to_str().unwrap();
    let mut clone = s(&["git", "clone", "--local", "--no-hardlinks"]);
    clone.extend(s(extra));
    clone.extend(s(&["--", source, p]));
    vec![
        s(&["git", "-C", repo, "rev-parse", "--git-dir"]),
        clone,
        s(&[
            "git",
            "-C",
            p,
            "-c",
            "core.hooksPath=/dev/null",
            "checkout",
            "-b",
            branch,
        ]),
    ]
}

#[tokio::test]
async fn s2_git_clone_runs_rev_parse_then_clone_then_checkout_exactly() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root.clone(), git.clone());

    let path = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    let expected_path = root.join("0192f0a1-2b3c-7d4e-8f50-123456789abc");
    assert_eq!(path, expected_path);
    assert_eq!(
        git.calls(),
        expected_calls(
            "/srv/repos/app",
            "/srv/repos/app",
            &[],
            &expected_path,
            "kestrel/run-123456789abc"
        )
    );
    assert!(path.is_dir());
}

#[tokio::test]
async fn s2_clone_never_uses_worktree_shared_or_reference() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());

    ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    let clones = git.clones();
    assert_eq!(clones.len(), 1, "{clones:?}");
    for bad in ["--shared", "--reference", "--dissociate", "worktree"] {
        assert!(
            git.calls().iter().flatten().all(|a| !a.starts_with(bad)),
            "{bad} used: {:?}",
            git.calls()
        );
    }
    assert!(clones[0].contains(&"--no-hardlinks".to_string()));
    assert!(clones[0].contains(&"--local".to_string()));
}

#[tokio::test]
async fn s2_the_branch_checkout_runs_before_prepare_returns_with_hooks_disabled() {
    // The fresh clone is trusted only until an agent can touch it, so the checkout must be done
    // inside `prepare`, and with the clone's hooks switched off.
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());

    ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    let checkouts: Vec<Vec<String>> = git.calls().into_iter().filter(|a| is_checkout(a)).collect();
    assert_eq!(checkouts.len(), 1, "{:?}", git.calls());
    assert!(
        checkouts[0]
            .windows(2)
            .any(|w| w[0] == "-c" && w[1] == "core.hooksPath=/dev/null"),
        "{:?}",
        checkouts[0]
    );
}

#[tokio::test]
async fn s2_depth_clones_a_file_url_with_depth_flag() {
    // git ignores --depth for a plain local path, so a shallow clone must use a file:// source.
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root.clone(), git.clone());
    let spec = WorkspaceSpec::GitClone {
        repo: repo(),
        depth: Some(1),
    };

    let path = ws.prepare(run_a(), &spec).await.unwrap();

    assert_eq!(
        git.calls(),
        expected_calls(
            "/srv/repos/app",
            "file:///srv/repos/app",
            &["--depth", "1"],
            &path,
            "kestrel/run-123456789abc"
        )
    );
}

#[tokio::test]
async fn s2_without_depth_the_source_is_the_plain_path() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());

    ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    let clone = &git.clones()[0];
    assert!(clone.contains(&"/srv/repos/app".to_string()), "{clone:?}");
    assert!(
        clone
            .iter()
            .all(|a| !a.starts_with("file://") && a != "--depth"),
        "{clone:?}"
    );
}

#[tokio::test]
async fn s2_two_runs_get_separate_clones_and_branches() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());

    let a = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();
    let b = ws.prepare(run_b(), &git_spec(&repo())).await.unwrap();

    assert_ne!(a, b);
    let clones = git.clones();
    assert_eq!(clones.len(), 2, "{clones:?}");
    let targets: HashSet<&String> = clones.iter().filter_map(|argv| argv.last()).collect();
    assert_eq!(targets.len(), 2, "clone targets shared: {clones:?}");
    let branches: HashSet<String> = git
        .calls()
        .into_iter()
        .filter(|argv| is_checkout(argv))
        .filter_map(|argv| argv.last().cloned())
        .collect();
    assert_eq!(
        branches,
        HashSet::from([
            "kestrel/run-123456789abc".to_string(),
            "kestrel/run-fedcba987654".to_string()
        ])
    );
}

#[tokio::test]
async fn s2_clone_failure_is_a_git_error_with_stderr_and_removes_the_half_made_dir() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::CloneFails);
    let ws = Workspaces::new(root, git.clone());

    let err = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap_err();

    match &err {
        WorkspaceError::Git {
            argv,
            status,
            stderr,
        } => {
            assert_eq!(*status, 128);
            assert!(stderr.contains("could not read"), "{stderr}");
            assert_eq!(argv[1], "clone", "{argv:?}");
        }
        other => panic!("expected Git, got {other:?}"),
    }
    assert!(err.to_string().contains("could not read"), "{err}");
    assert!(
        !ws.path_for(run_a()).exists(),
        "half-made clone directory left behind"
    );
    assert!(
        git.calls().iter().all(|a| !is_checkout(a)),
        "no checkout after a failed clone"
    );
}

#[tokio::test]
async fn s2_checkout_failure_is_a_git_error_and_removes_the_directory() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::CheckoutFails);
    let ws = Workspaces::new(root, git);

    let err = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap_err();

    match &err {
        WorkspaceError::Git { argv, stderr, .. } => {
            assert!(argv.iter().any(|a| a == "checkout"), "{argv:?}");
            assert!(stderr.contains("already exists"), "{stderr}");
        }
        other => panic!("expected Git, got {other:?}"),
    }
    assert!(
        !ws.path_for(run_a()).exists(),
        "a clone without its run branch must not be reused later"
    );
}

#[tokio::test]
async fn s2_after_a_failed_prepare_the_next_prepare_clones_again() {
    let (_tmp, root) = root();
    let failing = Workspaces::new(root.clone(), FakeGit::new(Mode::CheckoutFails));
    failing
        .prepare(run_a(), &git_spec(&repo()))
        .await
        .unwrap_err();

    let git = FakeGit::new(Mode::Ok);
    let ok = Workspaces::new(root, git.clone());
    ok.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    assert_eq!(git.clones().len(), 1, "{:?}", git.calls());
}

#[tokio::test]
async fn s2_git_not_startable_is_an_io_error() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::CannotSpawn));

    let err = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::Io { .. }), "{err:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn s2_non_utf8_repo_path_gets_its_own_error_and_runs_no_git() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());
    let bad = PathBuf::from(OsStr::from_bytes(b"/srv/repos/app-\xff"));

    let err = ws.prepare(run_a(), &git_spec(&bad)).await.unwrap_err();

    assert!(
        matches!(&err, WorkspaceError::NonUtf8Repo { repo } if repo == &bad),
        "{err:?}"
    );
    assert!(err.to_string().contains("UTF-8"), "{err}");
    assert!(
        !err.to_string().contains("not a git repository"),
        "misleading message: {err}"
    );
    assert!(git.calls().is_empty(), "git ran: {:?}", git.calls());
    assert!(!ws.path_for(run_a()).exists());
}

// ---------------------------------------------------------------------------------------------
// fetch_back (fake git): runs only from the source repo, never inside the clone.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn fetch_back_runs_exactly_one_hardened_fetch_from_the_source_repo() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root.clone(), git.clone());
    let path = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();
    let clone_git = path.join(".git");
    let before = git.calls().len();

    ws.fetch_back(run_a(), &repo()).await.unwrap();

    let calls = git.calls();
    assert_eq!(
        calls[before..],
        [s(&[
            "git",
            "-C",
            "/srv/repos/app",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "protocol.ext.allow=never",
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--upload-pack=git-upload-pack --strict",
            "--",
            clone_git.to_str().unwrap(),
            "refs/heads/kestrel/run-123456789abc:refs/heads/kestrel/run-123456789abc",
        ])]
    );
}

#[tokio::test]
async fn fetch_back_after_prepare_the_host_never_runs_git_with_the_clone_as_its_directory() {
    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());
    let path = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();
    let before = git.calls().len();

    ws.fetch_back(run_a(), &repo()).await.unwrap();

    let later: Vec<Vec<String>> = git.calls()[before..].to_vec();
    assert!(!later.is_empty());
    for argv in later {
        assert_eq!(argv[..3], s(&["git", "-C", "/srv/repos/app"]), "{argv:?}");
        // The clone may appear as the fetch source, but never as the directory git runs in.
        let in_clone = argv
            .windows(2)
            .any(|w| w[0] == "-C" && Path::new(&w[1]).starts_with(&path));
        assert!(!in_clone, "git ran inside the clone: {argv:?}");
    }
}

#[tokio::test]
async fn fetch_back_failure_is_a_git_error_with_stderr() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::NotARepo));

    let err = ws
        .fetch_back(run_a(), Path::new("/srv/repos/app"))
        .await
        .unwrap_err();

    match &err {
        WorkspaceError::Git {
            argv,
            status,
            stderr,
        } => {
            assert_eq!(*status, 128);
            assert!(stderr.contains("not a git repository"), "{stderr}");
            assert!(argv.iter().any(|a| a == "fetch"), "{argv:?}");
        }
        other => panic!("expected Git, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_back_git_not_startable_is_an_io_error() {
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::CannotSpawn));

    let err = ws
        .fetch_back(run_a(), Path::new("/srv/repos/app"))
        .await
        .unwrap_err();

    assert!(matches!(err, WorkspaceError::Io { .. }), "{err:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn fetch_back_with_a_non_utf8_repo_path_is_its_own_error() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let (_tmp, root) = root();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());
    let bad = PathBuf::from(OsStr::from_bytes(b"/srv/repos/app-\xff"));

    let err = ws.fetch_back(run_a(), &bad).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::NonUtf8Repo { .. }), "{err:?}");
    assert!(git.calls().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Scenario 3: a repo path that is not a git repo fails clearly, before any clone exists.
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
async fn s4_git_clone_prepare_twice_reuses_without_any_git() {
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
    // Only what the first prepare ran: a reused directory runs no git at all.
    assert_eq!(git.calls().len(), 3, "{:?}", git.calls());
    assert_eq!(git.clones().len(), 1, "{:?}", git.calls());
}

#[tokio::test]
async fn s4_reusing_a_clone_runs_no_git_at_all() {
    let (_tmp, root) = root();
    let path = root.join("0192f0a1-2b3c-7d4e-8f50-123456789abc");
    std::fs::create_dir_all(path.join(".git")).unwrap();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());

    let got = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap();

    assert_eq!(got, path);
    assert!(git.calls().is_empty(), "git ran: {:?}", git.calls());
}

#[tokio::test]
async fn s4_a_regular_file_at_the_run_path_is_refused_not_mounted() {
    for spec in [WorkspaceSpec::EmptyDir, git_spec(&repo())] {
        let (_tmp, root) = root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("0192f0a1-2b3c-7d4e-8f50-123456789abc");
        std::fs::write(&path, "i am a file").unwrap();
        let git = FakeGit::new(Mode::Ok);
        let ws = Workspaces::new(root, git.clone());

        let err = ws.prepare(run_a(), &spec).await.unwrap_err();

        assert!(
            matches!(&err, WorkspaceError::BadExisting { path: p, .. } if p == &path),
            "{spec:?}: {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains(path.to_str().unwrap()), "{msg}");
        assert!(msg.contains("not a directory"), "{msg}");
        assert!(git.calls().is_empty(), "git ran: {:?}", git.calls());
        assert!(path.is_file(), "the refused path was touched");
    }
}

#[tokio::test]
async fn s4_a_git_clone_directory_without_dot_git_is_refused_not_mounted() {
    let (_tmp, root) = root();
    let path = root.join("0192f0a1-2b3c-7d4e-8f50-123456789abc");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("half-made.txt"), "x").unwrap();
    let git = FakeGit::new(Mode::Ok);
    let ws = Workspaces::new(root, git.clone());

    let err = ws.prepare(run_a(), &git_spec(&repo())).await.unwrap_err();

    assert!(
        matches!(&err, WorkspaceError::BadExisting { path: p, .. } if p == &path),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains(path.to_str().unwrap()), "{msg}");
    assert!(msg.contains(".git"), "{msg}");
    assert!(git.calls().is_empty(), "git ran: {:?}", git.calls());
    assert!(
        path.join("half-made.txt").exists(),
        "the refused dir was modified"
    );
}

#[tokio::test]
async fn s4_an_empty_dir_workspace_without_dot_git_is_still_reused() {
    // The `.git` rule is for git clones only.
    let (_tmp, root) = root();
    let ws = Workspaces::new(root, FakeGit::new(Mode::Ok));
    let a = ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap();

    assert_eq!(
        ws.prepare(run_a(), &WorkspaceSpec::EmptyDir).await.unwrap(),
        a
    );
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
        config: None,
        cpu_millis: 1000,
        memory_mb: 256,
        network: NetworkPolicy::None,
        labels: BTreeMap::new(),
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
async fn s5_git_clone_path_passes_sandbox_validation() {
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
