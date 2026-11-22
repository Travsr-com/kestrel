//! Issue #22 scenarios 1 to 6 against the real `git` binary: per-run `git clone --local
//! --no-hardlinks` workspaces and `Workspaces::fetch_back`.
//!
//! The agent is simulated by the test itself (same user, free to do anything inside its clone).
//! Scenario 1 in a real gVisor sandbox is `s6_agent_can_commit_in_its_clone` in `tests/e2e`.
//! Every test skips with a message when `git` is not installed.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use kestrel_core::workspace::Workspaces;
use kestrel_sandbox::{CommandOutput, CommandRunner, ProcessRunner, SpawnedProcess};
use kestrel_types::{RunId, WorkspaceSpec};
use tempfile::TempDir;
use uuid::Uuid;

/// A run id with a known tail (`123456789abc`).
fn run_a() -> RunId {
    RunId::from_uuid(Uuid::parse_str("0192f0a1-2b3c-7d4e-8f50-123456789abc").unwrap())
}

/// Same timestamp prefix as `run_a`, tail `fedcba987654`.
fn run_b() -> RunId {
    RunId::from_uuid(Uuid::parse_str("0192f0a1-2b3c-7d4e-8f50-fedcba987654").unwrap())
}

const BRANCH_A: &str = "kestrel/run-123456789abc";
const BRANCH_B: &str = "kestrel/run-fedcba987654";

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// Skips the calling test (with a message) when git is missing.
macro_rules! need_git {
    () => {
        if !git_available() {
            eprintln!("SKIPPED {}: `git` is not on PATH", module_path!());
            return;
        }
    };
}

/// Runs git in `dir` as the test (or the simulated agent) and returns trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_raw(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_raw(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Kestrel Test",
            "-c",
            "user.email=test@kestrel.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git runs")
}

fn commit_file(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-q", "-m", &format!("add {name}")]);
}

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    root: PathBuf,
}

/// A source repo on `main` with `commits` commits, and a workspaces root next to it.
fn fixture(commits: usize) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("source");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    for i in 0..commits {
        commit_file(&repo, &format!("file{i}.txt"), &format!("content {i}\n"));
    }
    let root = tmp.path().join("workspaces");
    Fixture {
        _tmp: tmp,
        repo,
        root,
    }
}

fn clone_spec(repo: &Path, depth: Option<u32>) -> WorkspaceSpec {
    WorkspaceSpec::GitClone {
        repo: repo.to_path_buf(),
        depth,
    }
}

fn workspaces(f: &Fixture) -> Workspaces {
    Workspaces::new(f.root.clone(), Arc::new(ProcessRunner))
}

fn branches(repo: &Path) -> Vec<String> {
    git(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Scenario 1: git works inside the clone; the commit stays in the clone (the real sandbox
// version of this is the e2e test `s6_agent_can_commit_in_its_clone`).
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s1_commit_in_the_clone_lands_on_the_run_branch_and_only_there() {
    need_git!();
    let f = fixture(2);
    let ws = workspaces(&f);
    let source_head = git(&f.repo, &["rev-parse", "HEAD"]);

    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, None))
        .await
        .unwrap();

    // A self-contained checkout: `.git` is a directory, not a pointer into the source repo.
    assert!(clone.join(".git").is_dir(), "not a self-contained clone");
    assert_eq!(
        git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]),
        BRANCH_A
    );
    assert_eq!(
        git(&clone, &["status", "--porcelain"]),
        "",
        "clean checkout"
    );
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), source_head);

    commit_file(&clone, "agent.txt", "work\n");

    assert_ne!(git(&clone, &["rev-parse", "HEAD"]), source_head);
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), source_head);
    assert_eq!(branches(&f.repo), vec!["main"], "source branches untouched");
    assert!(!f.repo.join("agent.txt").exists());
}

#[tokio::test]
async fn s1_the_clone_has_no_alternates_file_and_no_worktree_metadata() {
    need_git!();
    let f = fixture(2);
    let ws = workspaces(&f);

    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, None))
        .await
        .unwrap();

    assert!(
        !clone.join(".git/objects/info/alternates").exists(),
        "alternates point at a host path outside the mount"
    );
    assert!(!files_under(&clone.join(".git/objects")).is_empty());
    assert!(!clone.join(".git/worktrees").exists());
}

// ---------------------------------------------------------------------------------------------
// Scenario 2: two parallel runs on one repo are isolated.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s2_two_parallel_runs_are_isolated_and_the_source_branches_are_untouched() {
    need_git!();
    let f = fixture(2);
    let ws = workspaces(&f);
    let spec = clone_spec(&f.repo, None);
    let source_head = git(&f.repo, &["rev-parse", "HEAD"]);

    let (a, b) = tokio::join!(ws.prepare(run_a(), &spec), ws.prepare(run_b(), &spec));
    let (a, b) = (a.unwrap(), b.unwrap());

    assert_ne!(a, b);
    assert_eq!(git(&a, &["rev-parse", "--abbrev-ref", "HEAD"]), BRANCH_A);
    assert_eq!(git(&b, &["rev-parse", "--abbrev-ref", "HEAD"]), BRANCH_B);

    commit_file(&a, "only-in-a.txt", "a\n");
    commit_file(&b, "only-in-b.txt", "b\n");

    assert!(!b.join("only-in-a.txt").exists(), "file leaked into run b");
    assert!(!a.join("only-in-b.txt").exists(), "file leaked into run a");
    assert_eq!(branches(&a), vec![BRANCH_A, "main"]);
    assert_eq!(branches(&b), vec![BRANCH_B, "main"]);
    assert!(
        !git_raw(&b, &["cat-file", "-e", &git(&a, &["rev-parse", "HEAD"])])
            .status
            .success(),
        "run b's object store has run a's commit"
    );
    assert_eq!(branches(&f.repo), vec!["main"], "source branches untouched");
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), source_head);
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn s2_prepare_twice_for_one_run_keeps_the_agents_commit() {
    need_git!();
    let f = fixture(1);
    let ws = workspaces(&f);
    let spec = clone_spec(&f.repo, None);
    let a = ws.prepare(run_a(), &spec).await.unwrap();
    commit_file(&a, "agent.txt", "work\n");
    let head = git(&a, &["rev-parse", "HEAD"]);

    let again = ws.prepare(run_a(), &spec).await.unwrap();

    assert_eq!(again, a);
    assert_eq!(
        git(&a, &["rev-parse", "HEAD"]),
        head,
        "reuse must not re-clone"
    );
    assert_eq!(git(&a, &["rev-parse", "--abbrev-ref", "HEAD"]), BRANCH_A);
}

// ---------------------------------------------------------------------------------------------
// Scenario 3: the agent (same user) wrecks its clone's .git; the source repo is unaffected.
// ---------------------------------------------------------------------------------------------

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn s3_rewriting_and_chmodding_the_clones_git_dir_leaves_the_source_repo_intact() {
    need_git!();
    let f = fixture(3);
    // Packed objects and loose ones, so both kinds of object file are covered.
    git(&f.repo, &["gc", "-q"]);
    commit_file(&f.repo, "loose.txt", "loose\n");
    let ws = workspaces(&f);

    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, None))
        .await
        .unwrap();

    // --no-hardlinks: every object file the clone has is a separate inode from the source's.
    let src_objects = f.repo.join(".git/objects");
    let mut compared = 0;
    for file in files_under(&clone.join(".git/objects")) {
        let rel = file.strip_prefix(clone.join(".git/objects")).unwrap();
        let twin = src_objects.join(rel);
        if !twin.is_file() {
            continue;
        }
        let (c, s) = (
            std::fs::metadata(&file).unwrap(),
            std::fs::metadata(&twin).unwrap(),
        );
        assert_ne!(
            c.ino(),
            s.ino(),
            "hardlinked object file: {}",
            rel.display()
        );
        assert_eq!(
            s.nlink(),
            1,
            "source object has extra links: {}",
            rel.display()
        );
        compared += 1;
    }
    assert!(compared >= 3, "compared only {compared} object files");

    let modes: Vec<(PathBuf, u32)> = files_under(&f.repo.join(".git"))
        .into_iter()
        .map(|p| {
            let m = std::fs::metadata(&p).unwrap().permissions().mode();
            (p, m)
        })
        .collect();

    // The agent: rewrite every file under its .git with garbage, then chmod it to nothing.
    for file in files_under(&clone.join(".git")) {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&file, b"garbage from a hostile agent").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    }

    let fsck = git_raw(&f.repo, &["fsck", "--full", "--strict"]);
    assert!(
        fsck.status.success(),
        "source repo is corrupted: {}{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
    assert_eq!(branches(&f.repo), vec!["main"]);
    for (path, mode) in modes {
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode(),
            mode,
            "mode of {} changed",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario 4: a hostile clone config cannot run anything on the host during fetch_back.
// ---------------------------------------------------------------------------------------------

/// Records every argv, then runs the real command.
struct Recording {
    calls: Mutex<Vec<Vec<String>>>,
}

impl Recording {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl CommandRunner for Recording {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
        ProcessRunner.run(argv).await
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        panic!("workspaces never spawn: {argv:?}");
    }
}

/// An executable shell script that creates `marker` when it runs.
fn trap(path: &Path, marker: &Path) {
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\ntouch '{}'\ncat >/dev/null 2>&1\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn s4_a_hostile_clone_config_and_hooks_never_run_during_fetch_back() {
    need_git!();
    let f = fixture(2);
    let recorder = Arc::new(Recording {
        calls: Mutex::new(Vec::new()),
    });
    let ws = Workspaces::new(f.root.clone(), recorder.clone());
    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, None))
        .await
        .unwrap();
    commit_file(&clone, "agent.txt", "work\n");

    // The agent plants traps. Each one drops its own marker file when something runs it.
    let traps = f.root.parent().unwrap().join("traps");
    std::fs::create_dir(&traps).unwrap();
    let marker = |name: &str| traps.join(format!("{name}.marker"));
    let hooks_dir = traps.join("hooks");
    std::fs::create_dir(&hooks_dir).unwrap();
    let fsmonitor = traps.join("fsmonitor.sh");
    trap(&fsmonitor, &marker("fsmonitor"));
    let pager = traps.join("pager.sh");
    trap(&pager, &marker("pager"));
    let pack_hook = traps.join("pack-hook.sh");
    trap(&pack_hook, &marker("packhook"));
    let cfg = |k: &str, v: &str| git(&clone, &["config", k, v]);
    cfg("core.fsmonitor", fsmonitor.to_str().unwrap());
    cfg("core.hooksPath", hooks_dir.to_str().unwrap());
    cfg("core.pager", pager.to_str().unwrap());
    cfg("pager.fetch", "true");
    cfg("uploadpack.packObjectsHook", pack_hook.to_str().unwrap());
    cfg(
        "alias.fetch-evil",
        &format!("!touch '{}'", marker("alias").display()),
    );
    let hook_names = [
        "pre-commit",
        "post-checkout",
        "post-commit",
        "post-update",
        "pre-push",
        "reference-transaction",
        "post-fetch",
        "update",
        "pre-receive",
        "post-receive",
    ];
    // The clone has no template, so no `.git/hooks`; the agent makes it.
    std::fs::create_dir_all(clone.join(".git/hooks")).unwrap();
    for name in hook_names {
        trap(&hooks_dir.join(name), &marker(&format!("hookspath-{name}")));
        trap(
            &clone.join(".git/hooks").join(name),
            &marker(&format!("githooks-{name}")),
        );
    }
    // The source repo's own hooks are the user's, but they must not run on a fetch_back either.
    for name in ["reference-transaction", "post-update", "update"] {
        trap(
            &f.repo.join(".git/hooks").join(name),
            &marker(&format!("source-{name}")),
        );
    }

    // Prove the traps are live: a naive host that ran git inside the clone would trip them.
    git(&clone, &["status"]);
    assert!(
        marker("fsmonitor").exists(),
        "test is broken: core.fsmonitor did not run for `git status` in the clone"
    );
    git(&clone, &["checkout", "-q", "-b", "probe"]);
    assert!(
        marker("hookspath-post-checkout").exists(),
        "test is broken: the hooks in core.hooksPath did not run for `git checkout`"
    );
    git(&clone, &["checkout", "-q", BRANCH_A]);
    git(&clone, &["branch", "-q", "-D", "probe"]);
    for entry in std::fs::read_dir(&traps).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "marker") {
            std::fs::remove_file(p).unwrap();
        }
    }
    let before = recorder.calls().len();

    ws.fetch_back(run_a(), &f.repo).await.unwrap();

    let leaked: Vec<String> = std::fs::read_dir(&traps)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".marker"))
        .collect();
    assert!(
        leaked.is_empty(),
        "hook or config code ran on the host: {leaked:?}"
    );
    assert_eq!(
        git(&f.repo, &["rev-parse", BRANCH_A]),
        git(&clone, &["rev-parse", BRANCH_A]),
        "the fetch itself still worked"
    );

    // And every git call fetch_back made ran from the source repo.
    let repo = f.repo.to_str().unwrap();
    let after = recorder.calls()[before..].to_vec();
    assert!(!after.is_empty());
    for argv in after {
        assert_eq!(argv[..3], ["git", "-C", repo], "{argv:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario 5: a finished run's branch can be fetched into the source repo.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s5_fetch_back_creates_the_run_branch_in_the_source_repo_with_the_clones_commit() {
    need_git!();
    let f = fixture(2);
    let ws = workspaces(&f);
    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, None))
        .await
        .unwrap();
    commit_file(&clone, "agent.txt", "work\n");
    let commit = git(&clone, &["rev-parse", "HEAD"]);
    let source_head = git(&f.repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        branches(&f.repo),
        vec!["main"],
        "no run branch before fetch_back"
    );

    ws.fetch_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(git(&f.repo, &["rev-parse", BRANCH_A]), commit);
    assert_eq!(branches(&f.repo), vec![BRANCH_A, "main"]);
    assert_eq!(
        git(&f.repo, &["show", &format!("{BRANCH_A}:agent.txt")]),
        "work"
    );
    // The user's own branch, checkout and working tree are not touched.
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), source_head);
    assert_eq!(git(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
    assert!(!f.repo.join("agent.txt").exists());
    assert_eq!(
        git(&f.repo, &["tag", "--list"]),
        "",
        "fetch_back must not import tags"
    );
}

#[tokio::test]
async fn s5_a_second_fetch_back_fast_forwards_the_branch() {
    need_git!();
    let f = fixture(1);
    let ws = workspaces(&f);
    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, None))
        .await
        .unwrap();
    commit_file(&clone, "one.txt", "1\n");
    ws.fetch_back(run_a(), &f.repo).await.unwrap();
    commit_file(&clone, "two.txt", "2\n");

    ws.fetch_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        git(&f.repo, &["rev-parse", BRANCH_A]),
        git(&clone, &["rev-parse", "HEAD"])
    );
}

#[tokio::test]
async fn s5_fetch_back_for_a_run_that_was_never_prepared_is_an_error() {
    need_git!();
    let f = fixture(1);
    let ws = workspaces(&f);

    let err = ws.fetch_back(run_b(), &f.repo).await.unwrap_err();

    assert!(!err.to_string().is_empty());
    assert_eq!(branches(&f.repo), vec!["main"]);
}

// ---------------------------------------------------------------------------------------------
// Scenario 6: clone_depth gives a shallow clone.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s6_depth_one_gives_a_shallow_clone_on_the_run_branch() {
    need_git!();
    let f = fixture(4);
    let ws = workspaces(&f);

    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, Some(1)))
        .await
        .unwrap();

    assert_eq!(
        git(&clone, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
    assert_eq!(git(&clone, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(
        git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]),
        BRANCH_A
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("file3.txt")).unwrap(),
        "content 3\n"
    );
}

#[tokio::test]
async fn s6_without_depth_the_clone_has_full_history() {
    need_git!();
    let f = fixture(4);
    let ws = workspaces(&f);

    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, None))
        .await
        .unwrap();

    assert_eq!(
        git(&clone, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
    assert_eq!(git(&clone, &["rev-list", "--count", "HEAD"]), "4");
}

#[tokio::test]
async fn s6_a_shallow_clones_branch_can_be_fetched_back() {
    need_git!();
    let f = fixture(4);
    let ws = workspaces(&f);
    let clone = ws
        .prepare(run_a(), &clone_spec(&f.repo, Some(1)))
        .await
        .unwrap();
    commit_file(&clone, "agent.txt", "work\n");

    ws.fetch_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        git(&f.repo, &["rev-parse", BRANCH_A]),
        git(&clone, &["rev-parse", "HEAD"])
    );
    let fsck = git_raw(&f.repo, &["fsck", "--full"]);
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
