//! Issue #93: `Workspaces::bring_back` brings a run's branch home into the source repository when
//! the clone has commits the source does not.
//!
//! Scenarios (all `s<N>_...`, numbered as in the issue):
//!
//! 1. a clone with a new commit: `kestrel/run-<id>` arrives in the source repo
//! 2. six clones that changed the same file: six branches, none overwrites another or the user's
//! 3. a clone with no new commits: nothing is fetched, nothing fails
//! 4. a source repo the user changed meanwhile (new commits, a dirty tree): untouched
//! 5. the hardening of #52 applies (fsck on fetch, scrubbed environment, `.git` checks), and the
//!    clone cannot point the fetch at another repository: git reads exactly `<clone>/.git` with a
//!    strict upload-pack, `.git` holds no `commondir`, `.git`, alternates or links, the refspec
//!    names the branch in full, and a git call that hangs is stopped
//!
//! Real-git tests skip with a message when `git` is missing. The rest use a scripted runner.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use kestrel_core::workspace::{BroughtBack, WorkspaceError, Workspaces, git_env_scrub};
use kestrel_sandbox::{CommandOutput, CommandRunner, EnvScrub, ProcessRunner, SpawnedProcess};
use kestrel_types::{RunId, WorkspaceSpec};
use tempfile::TempDir;
use uuid::Uuid;

fn run_a() -> RunId {
    RunId::from_uuid(Uuid::parse_str("0192f0a1-2b3c-7d4e-8f50-123456789abc").unwrap())
}

const BRANCH_A: &str = "kestrel/run-123456789abc";

/// A run id whose tail is `n`, so six runs get six known branches.
fn run_n(n: u32) -> RunId {
    RunId::from_uuid(Uuid::parse_str(&format!("0192f0a1-2b3c-7d4e-8f50-{n:012x}")).unwrap())
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

macro_rules! need_git {
    () => {
        if !git_available() {
            eprintln!("SKIPPED {}: `git` is not on PATH", module_path!());
            return;
        }
    };
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
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .output()
        .expect("git runs")
}

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

fn commit_file(dir: &Path, name: &str, body: &str) -> String {
    std::fs::write(dir.join(name), body).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-q", "-m", &format!("write {name}")]);
    git(dir, &["rev-parse", "HEAD"])
}

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("source");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    commit_file(&repo, "shared.txt", "base\n");
    let root = tmp.path().join("workspaces");
    Fixture {
        _tmp: tmp,
        repo,
        root,
    }
}

fn clone_spec(repo: &Path) -> WorkspaceSpec {
    WorkspaceSpec::GitClone {
        repo: repo.to_path_buf(),
        depth: None,
    }
}

fn real(f: &Fixture) -> Workspaces {
    Workspaces::new(f.root.clone(), Arc::new(ProcessRunner))
}

/// `refname objectname` of every ref under `refs/heads/kestrel/` in `repo`.
fn kestrel_refs(repo: &Path) -> Vec<String> {
    let out = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/kestrel/",
        ],
    );
    out.lines().map(str::to_string).collect()
}

// ---------------------------------------------------------------------------------------------
// Scenario 1: a clone with a new commit comes home as its run branch.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s1_a_new_commit_in_the_clone_arrives_as_the_run_branch() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    let tip = commit_file(&clone, "agent.txt", "work\n");

    let got = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::Fetched {
            branch: BRANCH_A.into(),
            commit: tip.clone(),
        }
    );
    assert_eq!(git(&f.repo, &["rev-parse", BRANCH_A]), tip);
    assert_eq!(
        git(&f.repo, &["show", &format!("{BRANCH_A}:agent.txt")]),
        "work"
    );
}

#[tokio::test]
async fn s1_bringing_back_twice_reports_already_fetched_and_moves_nothing() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    let tip = commit_file(&clone, "agent.txt", "work\n");
    ws.bring_back(run_a(), &f.repo).await.unwrap();
    let refs = kestrel_refs(&f.repo);

    let again = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        again,
        BroughtBack::AlreadyFetched {
            branch: BRANCH_A.into(),
            commit: tip,
        }
    );
    assert_eq!(kestrel_refs(&f.repo), refs);
}

#[tokio::test]
async fn s1_a_run_branch_the_user_moved_ahead_is_already_fetched_and_kept() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    let tip = commit_file(&clone, "agent.txt", "work\n");
    ws.bring_back(run_a(), &f.repo).await.unwrap();
    // The user reviews the branch and adds a commit of their own on top.
    git(&f.repo, &["checkout", "-q", BRANCH_A]);
    let mine = commit_file(&f.repo, "review.txt", "fixup\n");
    git(&f.repo, &["checkout", "-q", "main"]);

    let got = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::AlreadyFetched {
            branch: BRANCH_A.into(),
            commit: tip,
        }
    );
    assert_eq!(git(&f.repo, &["rev-parse", BRANCH_A]), mine);
}

#[tokio::test]
async fn s1_more_commits_after_a_fetch_fast_forward_the_branch() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    commit_file(&clone, "agent.txt", "one\n");
    ws.bring_back(run_a(), &f.repo).await.unwrap();
    let tip = commit_file(&clone, "agent.txt", "two\n");

    let got = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::Fetched {
            branch: BRANCH_A.into(),
            commit: tip.clone(),
        }
    );
    assert_eq!(git(&f.repo, &["rev-parse", BRANCH_A]), tip);
}

#[tokio::test]
async fn s1_a_fetch_writes_no_fetch_head_in_the_source_repo() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    commit_file(&clone, "agent.txt", "work\n");

    ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert!(
        !f.repo.join(".git").join("FETCH_HEAD").exists(),
        "the user's FETCH_HEAD was written"
    );
}

// ---------------------------------------------------------------------------------------------
// Scenario 2: six clones that changed the same file give six branches.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s2_six_clones_editing_the_same_file_arrive_as_six_branches() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let main = git(&f.repo, &["rev-parse", "main"]);
    let mut tips = Vec::new();
    for n in 1..=6 {
        let clone = ws.prepare(run_n(n), &clone_spec(&f.repo)).await.unwrap();
        tips.push(commit_file(&clone, "shared.txt", &format!("kid {n}\n")));
    }

    // All six at once, as six children ending together would.
    let results =
        futures::future::join_all((1..=6).map(|n| ws.bring_back(run_n(n), &f.repo))).await;

    for (i, result) in results.into_iter().enumerate() {
        let n = u32::try_from(i).unwrap() + 1;
        let branch = Workspaces::branch_for(run_n(n));
        assert_eq!(
            result.unwrap(),
            BroughtBack::Fetched {
                branch: branch.clone(),
                commit: tips[i].clone(),
            }
        );
        assert_eq!(
            git(&f.repo, &["show", &format!("{branch}:shared.txt")]),
            format!("kid {n}")
        );
        assert_eq!(git(&f.repo, &["rev-parse", &format!("{branch}^")]), main);
    }
    assert_eq!(kestrel_refs(&f.repo).len(), 6);
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), main);
    assert_eq!(git(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(
        std::fs::read_to_string(f.repo.join("shared.txt")).unwrap(),
        "base\n"
    );
}

// ---------------------------------------------------------------------------------------------
// Scenario 3: no new commits, nothing fetched.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s3_a_clone_without_new_commits_fetches_nothing() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    // Uncommitted work is not a commit.
    std::fs::write(clone.join("scratch.txt"), "draft\n").unwrap();
    let main = git(&f.repo, &["rev-parse", "main"]);

    let got = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::NothingNew {
            branch: BRANCH_A.into(),
            commit: main,
        }
    );
    assert!(kestrel_refs(&f.repo).is_empty(), "a branch was created");
    assert!(!f.repo.join(".git").join("FETCH_HEAD").exists());
}

#[tokio::test]
async fn s3_a_clone_whose_branch_is_behind_the_users_main_fetches_nothing() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    // The user moved on; the clone's tip is an ancestor of main now.
    commit_file(&f.repo, "user.txt", "mine\n");

    let got = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert!(matches!(got, BroughtBack::NothingNew { .. }), "{got:?}");
    assert!(kestrel_refs(&f.repo).is_empty());
}

#[tokio::test]
async fn s3_a_clone_without_its_run_branch_fetches_nothing() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    // The agent committed on a branch of its own and deleted the run branch.
    git(&clone, &["checkout", "-q", "-b", "elsewhere"]);
    commit_file(&clone, "agent.txt", "work\n");
    git(&clone, &["branch", "-q", "-D", BRANCH_A]);

    let got = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::NoBranch {
            branch: BRANCH_A.into()
        }
    );
    assert!(kestrel_refs(&f.repo).is_empty());
}

#[tokio::test]
async fn s3_a_run_without_a_clone_runs_no_git() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Scripted::new();
    let ws = Workspaces::new(tmp.path().join("workspaces"), fake.clone());

    let got = ws.bring_back(run_a(), tmp.path()).await.unwrap();

    assert_eq!(got, BroughtBack::NoClone);
    assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
}

// ---------------------------------------------------------------------------------------------
// Scenario 4: the user's working tree, index, HEAD and branches are never touched.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn s4_a_dirty_source_with_new_commits_is_left_as_it_was() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    let tip = commit_file(&clone, "shared.txt", "agent\n");
    // Meanwhile the user commits, stages a change and leaves another unstaged, on a branch of
    // their own.
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_file(&f.repo, "user.txt", "mine\n");
    std::fs::write(f.repo.join("staged.txt"), "staged\n").unwrap();
    git(&f.repo, &["add", "staged.txt"]);
    std::fs::write(f.repo.join("shared.txt"), "user edit\n").unwrap();
    let heads = git(
        &f.repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/",
        ],
    );
    let status = git(&f.repo, &["status", "--porcelain"]);
    let index = std::fs::read(f.repo.join(".git").join("index")).unwrap();
    let head = std::fs::read_to_string(f.repo.join(".git").join("HEAD")).unwrap();

    let got = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert!(matches!(got, BroughtBack::Fetched { .. }), "{got:?}");
    assert_eq!(git(&f.repo, &["rev-parse", BRANCH_A]), tip);
    let after = git(
        &f.repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/",
        ],
    );
    let added = format!("refs/heads/{BRANCH_A} {tip}");
    let without: Vec<&str> = after.lines().filter(|l| *l != added).collect();
    assert_eq!(without, heads.lines().collect::<Vec<_>>(), "a branch moved");
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), status);
    assert_eq!(
        std::fs::read(f.repo.join(".git").join("index")).unwrap(),
        index
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.join(".git").join("HEAD")).unwrap(),
        head
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.join("shared.txt")).unwrap(),
        "user edit\n"
    );
}

#[tokio::test]
async fn s4_a_run_branch_the_user_has_checked_out_is_refused_and_untouched() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    commit_file(&clone, "agent.txt", "one\n");
    ws.bring_back(run_a(), &f.repo).await.unwrap();
    // The user checks the run branch out to look at it; the agent's clone moves on.
    git(&f.repo, &["checkout", "-q", BRANCH_A]);
    let theirs = git(&f.repo, &["rev-parse", "HEAD"]);
    commit_file(&clone, "agent.txt", "two\n");

    let err = ws.bring_back(run_a(), &f.repo).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::Git { .. }), "{err:?}");
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), theirs);
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(f.repo.join("agent.txt")).unwrap(),
        "one\n"
    );
}

#[tokio::test]
async fn s4_a_rewritten_clone_branch_errors_and_keeps_the_fetched_branch() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    let first = commit_file(&clone, "agent.txt", "one\n");
    ws.bring_back(run_a(), &f.repo).await.unwrap();
    git(&clone, &["reset", "-q", "--hard", "HEAD^"]);
    commit_file(&clone, "other.txt", "rewritten\n");

    let err = ws.bring_back(run_a(), &f.repo).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::Git { .. }), "{err:?}");
    assert_eq!(git(&f.repo, &["rev-parse", BRANCH_A]), first);
}

// ---------------------------------------------------------------------------------------------
// Scenario 5: the hardening of #52 applies.
// ---------------------------------------------------------------------------------------------

/// Writes a commit on `branch` in `clone` whose tree has two entries named `a`: storable, but
/// `git fsck` calls it an error.
fn plant_bad_commit(clone: &Path, branch: &str) {
    let blob_file = clone.join("..").join("blob.bin");
    std::fs::write(&blob_file, b"hi").unwrap();
    let hex = git(clone, &["hash-object", "-w", blob_file.to_str().unwrap()]);
    let raw_id: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let mut entry = b"100644 a\0".to_vec();
    entry.extend(&raw_id);
    let tree_bytes = [entry.clone(), entry].concat();
    let tree_file = clone.join("..").join("bad-tree.bin");
    std::fs::write(&tree_file, tree_bytes).unwrap();
    let tree = git(
        clone,
        &[
            "hash-object",
            "-t",
            "tree",
            "-w",
            "--literally",
            tree_file.to_str().unwrap(),
        ],
    );
    let commit = git(
        clone,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "bad object"],
    );
    git(
        clone,
        &["update-ref", &format!("refs/heads/{branch}"), &commit],
    );
    let fsck = git_raw(clone, &["fsck", "--strict"]);
    assert!(!fsck.status.success(), "test is broken: the object is fine");
}

#[tokio::test]
async fn s5_an_object_that_fails_fsck_is_refused_and_the_source_gets_no_ref() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    plant_bad_commit(&clone, BRANCH_A);
    let before = git(&f.repo, &["for-each-ref"]);

    let err = ws.bring_back(run_a(), &f.repo).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::Git { .. }), "{err:?}");
    assert!(kestrel_refs(&f.repo).is_empty());
    assert_eq!(git(&f.repo, &["for-each-ref"]), before);
}

#[tokio::test]
async fn s5_a_gitfile_clone_is_refused_before_any_git_call() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspaces");
    let clone = root.join(run_a().to_string());
    std::fs::create_dir_all(&clone).unwrap();
    std::fs::write(clone.join(".git"), "gitdir: /somewhere/else\n").unwrap();
    let fake = Scripted::new();
    let ws = Workspaces::new(root, fake.clone());

    let err = ws.bring_back(run_a(), tmp.path()).await.unwrap_err();

    assert!(
        matches!(err, WorkspaceError::GitDirNotADirectory { .. }),
        "{err:?}"
    );
    assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
}

#[tokio::test]
async fn s5_a_clone_with_object_alternates_is_refused_before_any_git_call() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspaces");
    let info = root
        .join(run_a().to_string())
        .join(".git")
        .join("objects")
        .join("info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("alternates"), "/elsewhere/objects\n").unwrap();
    let fake = Scripted::new();
    let ws = Workspaces::new(root, fake.clone());

    let err = ws.bring_back(run_a(), tmp.path()).await.unwrap_err();

    assert!(
        matches!(err, WorkspaceError::AlternatesPresent { .. }),
        "{err:?}"
    );
    assert!(fake.argvs().is_empty());
}

#[tokio::test]
async fn s5_a_relative_repo_is_refused_before_any_git_call() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspaces");
    std::fs::create_dir_all(root.join(run_a().to_string()).join(".git")).unwrap();
    let fake = Scripted::new();
    let ws = Workspaces::new(root, fake.clone());

    let err = ws
        .bring_back(run_a(), Path::new("relative/repo"))
        .await
        .unwrap_err();

    assert!(
        matches!(err, WorkspaceError::RelativeRepo { .. }),
        "{err:?}"
    );
    assert!(fake.argvs().is_empty());
}

#[tokio::test]
async fn s5_every_git_call_runs_in_the_source_repo_hardened_and_scrubbed() {
    let s = Setup::new();
    s.fake
        .answer("ls-remote", 0, &format!("{NEW}\trefs/heads/{BRANCH_A}\n"));
    s.fake.answer("rev-parse", 1, "");

    let got = s.ws.bring_back(run_a(), &s.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::Fetched {
            branch: BRANCH_A.into(),
            commit: NEW.into(),
        }
    );
    let calls = s.fake.calls();
    let names: Vec<&str> = calls.iter().map(|(a, _)| subcommand(a)).collect();
    assert_eq!(names, ["ls-remote", "rev-parse", "fetch"]);
    let repo = s.repo.to_str().unwrap();
    for (argv, env) in &calls {
        assert_eq!(argv[..3], ["git", "-C", repo], "{argv:?}");
        let at = argv.iter().position(|a| a == subcommand(argv)).unwrap();
        let options = &argv[..at];
        for pair in [
            "core.hooksPath=/dev/null",
            "transfer.fsckObjects=true",
            "protocol.ext.allow=never",
        ] {
            assert!(
                options.windows(2).any(|w| w[0] == "-c" && w[1] == pair),
                "{pair} missing: {argv:?}"
            );
        }
        assert_eq!(env.as_ref(), Some(&git_env_scrub()), "{argv:?}");
        // The clone's git dir is only ever an argument after `--`, never the directory git runs
        // in, and the clone root is never named: upload-pack would pick a git dir there itself.
        let clone_git = s.clone.join(".git");
        let clone_git = clone_git.to_str().unwrap();
        if let Some(at) = argv.iter().position(|a| a == clone_git) {
            assert!(
                argv[..at].iter().any(|a| a == "--"),
                "the clone before `--`: {argv:?}"
            );
        }
        assert!(
            !argv.iter().any(|a| a == s.clone.to_str().unwrap()),
            "the clone root was given to git: {argv:?}"
        );
    }
    let clone_git = s.clone.join(".git");
    let clone_git = clone_git.to_str().unwrap();
    let run_ref = format!("refs/heads/{BRANCH_A}");
    let fetch = &calls[2].0;
    let at = fetch.iter().position(|a| a == "fetch").unwrap();
    assert_eq!(
        fetch[at..],
        [
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--upload-pack=git-upload-pack --strict",
            "--",
            clone_git,
            &format!("{run_ref}:{run_ref}"),
        ]
    );
    let ls = &calls[0].0;
    let at = ls.iter().position(|a| a == "ls-remote").unwrap();
    assert_eq!(
        ls[at..],
        [
            "ls-remote",
            "--upload-pack=git-upload-pack --strict",
            "--",
            clone_git,
            &run_ref,
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Scenario 5, continued: the clone cannot point the fetch at another repository (review of #113).
// ---------------------------------------------------------------------------------------------

/// A repository elsewhere on the host, and a commit in it the source must never receive.
fn secret_repo(f: &Fixture) -> (PathBuf, String) {
    let secret = f.repo.parent().unwrap().join("secret");
    std::fs::create_dir(&secret).unwrap();
    git(&secret, &["init", "-q"]);
    let commit = commit_file(&secret, "secret.txt", "do not leak\n");
    (secret, commit)
}

/// `path` as git reads it in a file such as `alternates` or `commondir`: forward slashes.
fn git_path(path: &Path) -> String {
    path.to_str().unwrap().replace('\\', "/")
}

/// Whether `repo` holds the commit `id`.
fn has_commit(repo: &Path, id: &str) -> bool {
    git_raw(repo, &["cat-file", "-e", &format!("{id}^{{commit}}")])
        .status
        .success()
}

/// Asserts `bring_back` refused the clone, and the source has no new ref and no `leaked` commit.
async fn assert_refused_and_untouched(ws: &Workspaces, f: &Fixture, leaked: &str) {
    let before = git(&f.repo, &["for-each-ref"]);

    let got = ws.bring_back(run_a(), &f.repo).await;

    assert!(got.is_err(), "an unsafe clone was fetched from: {got:?}");
    assert_eq!(git(&f.repo, &["for-each-ref"]), before);
    assert!(
        !has_commit(&f.repo, leaked),
        "another repository's commit reached the source"
    );
}

#[tokio::test]
async fn s5_a_bare_repo_at_the_clone_root_behind_an_empty_dot_git_is_refused() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    let (secret, leaked) = secret_repo(&f);
    // The agent turns its clone into a bare repo whose objects come from `secret`, with the run
    // branch at the secret commit, and leaves an empty `.git` in front of it.
    std::fs::remove_dir_all(&clone).unwrap();
    std::fs::create_dir(&clone).unwrap();
    git(&clone, &["init", "-q", "--bare"]);
    std::fs::write(
        clone.join("objects").join("info").join("alternates"),
        format!("{}\n", git_path(&secret.join(".git").join("objects"))),
    )
    .unwrap();
    git(
        &clone,
        &["update-ref", &format!("refs/heads/{BRANCH_A}"), &leaked],
    );
    std::fs::create_dir(clone.join(".git")).unwrap();

    assert_refused_and_untouched(&ws, &f, &leaked).await;
}

#[tokio::test]
async fn s5_a_commondir_pointing_at_another_repo_is_refused() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    let (secret, leaked) = secret_repo(&f);
    // With a `commondir`, git reads refs and objects from the other repo, which holds a branch
    // of the run's name.
    git(&secret, &["branch", BRANCH_A, &leaked]);
    std::fs::write(
        clone.join(".git").join("commondir"),
        format!("{}\n", git_path(&secret.join(".git"))),
    )
    .unwrap();

    assert_refused_and_untouched(&ws, &f, &leaked).await;
}

#[tokio::test]
async fn s5_a_planted_ref_or_tag_of_the_branch_name_cannot_shadow_the_run_branch() {
    need_git!();
    let f = fixture();
    let ws = real(&f);
    let clone = ws.prepare(run_a(), &clone_spec(&f.repo)).await.unwrap();
    // A short refspec would resolve `refs/<name>` and `refs/tags/<name>` before `refs/heads/`.
    let planted = commit_file(&clone, "planted.txt", "not the work\n");
    git(
        &clone,
        &["update-ref", &format!("refs/{BRANCH_A}"), &planted],
    );
    git(&clone, &["tag", BRANCH_A, &planted]);
    git(&clone, &["reset", "-q", "--hard", "HEAD^"]);
    let tip = commit_file(&clone, "agent.txt", "work\n");

    let got = ws.bring_back(run_a(), &f.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::Fetched {
            branch: BRANCH_A.into(),
            commit: tip.clone(),
        }
    );
    assert_eq!(
        git(&f.repo, &["rev-parse", &format!("refs/heads/{BRANCH_A}")]),
        tip
    );
    assert!(
        !has_commit(&f.repo, &planted),
        "the planted commit came home"
    );
}

/// Changes the `.git` of a clone that passes every other check with `plant`, then asserts
/// `bring_back` refuses it before any git call.
async fn refused_before_any_git_call(case: &str, plant: impl FnOnce(&Path)) {
    let s = Setup::new();
    plant(&s.clone.join(".git"));

    let got = s.ws.bring_back(run_a(), &s.repo).await;

    let err = got.expect_err(case);
    assert!(err.to_string().contains("refusing"), "{case}: {err}");
    assert!(s.fake.argvs().is_empty(), "{case}: {:?}", s.fake.argvs());
}

#[tokio::test]
async fn s5_a_git_dir_that_borrows_another_repo_is_refused_before_any_git_call() {
    refused_before_any_git_call("commondir", |git_dir| {
        std::fs::write(git_dir.join("commondir"), "/elsewhere/.git\n").unwrap();
    })
    .await;
    refused_before_any_git_call(".git/.git directory", |git_dir| {
        std::fs::create_dir(git_dir.join(".git")).unwrap();
    })
    .await;
    refused_before_any_git_call(".git/.git file", |git_dir| {
        std::fs::write(git_dir.join(".git"), "gitdir: /elsewhere/.git\n").unwrap();
    })
    .await;
    refused_before_any_git_call("http-alternates", |git_dir| {
        let info = git_dir.join("objects").join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("http-alternates"), "http://example.invalid/\n").unwrap();
    })
    .await;
}

#[tokio::test]
async fn s5_a_head_that_is_not_a_regular_file_is_refused_before_any_git_call() {
    refused_before_any_git_call("no HEAD", |git_dir| {
        std::fs::remove_file(git_dir.join("HEAD")).unwrap();
    })
    .await;
    refused_before_any_git_call("HEAD is a directory", |git_dir| {
        std::fs::remove_file(git_dir.join("HEAD")).unwrap();
        std::fs::create_dir(git_dir.join("HEAD")).unwrap();
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn s5_linked_objects_refs_or_packed_refs_are_refused_before_any_git_call() {
    use std::os::unix::fs::symlink;
    let elsewhere = tempfile::tempdir().unwrap();
    let away = elsewhere.path().to_path_buf();
    std::fs::create_dir_all(away.join("info")).unwrap();
    std::fs::write(away.join("file"), "x\n").unwrap();

    refused_before_any_git_call("HEAD link", |g| {
        std::fs::remove_file(g.join("HEAD")).unwrap();
        symlink(away.join("file"), g.join("HEAD")).unwrap();
    })
    .await;
    refused_before_any_git_call("objects link", |g| {
        symlink(&away, g.join("objects")).unwrap();
    })
    .await;
    refused_before_any_git_call("objects/info link", |g| {
        std::fs::create_dir(g.join("objects")).unwrap();
        symlink(away.join("info"), g.join("objects").join("info")).unwrap();
    })
    .await;
    refused_before_any_git_call("refs link", |g| {
        symlink(&away, g.join("refs")).unwrap();
    })
    .await;
    refused_before_any_git_call("refs/heads/kestrel link", |g| {
        std::fs::create_dir_all(g.join("refs").join("heads")).unwrap();
        symlink(&away, g.join("refs").join("heads").join("kestrel")).unwrap();
    })
    .await;
    refused_before_any_git_call("a ref file link", |g| {
        let kestrel = g.join("refs").join("heads").join("kestrel");
        std::fs::create_dir_all(&kestrel).unwrap();
        symlink(away.join("file"), kestrel.join("run-123456789abc")).unwrap();
    })
    .await;
    refused_before_any_git_call("packed-refs link", |g| {
        symlink(away.join("file"), g.join("packed-refs")).unwrap();
    })
    .await;
    // Below `objects` a link lets git read another repo's packs or loose objects.
    refused_before_any_git_call("objects/pack link", |g| {
        std::fs::create_dir(g.join("objects")).unwrap();
        symlink(&away, g.join("objects").join("pack")).unwrap();
    })
    .await;
    refused_before_any_git_call("loose object directory link", |g| {
        std::fs::create_dir(g.join("objects")).unwrap();
        symlink(&away, g.join("objects").join("ab")).unwrap();
    })
    .await;
    refused_before_any_git_call("pack file link", |g| {
        let pack = g.join("objects").join("pack");
        std::fs::create_dir_all(&pack).unwrap();
        symlink(away.join("file"), pack.join("pack-1.pack")).unwrap();
    })
    .await;
    // A linked reftable directory would hand git another repo's refs.
    refused_before_any_git_call("reftable link", |g| {
        symlink(&away, g.join("reftable")).unwrap();
    })
    .await;
}

#[tokio::test]
async fn s5_a_clone_with_real_refs_and_objects_directories_passes_the_checks() {
    let s = Setup::new();
    let git_dir = s.clone.join(".git");
    for dir in [
        "objects/info",
        "objects/pack",
        "refs/heads/kestrel",
        "refs/tags",
    ] {
        std::fs::create_dir_all(git_dir.join(dir)).unwrap();
    }
    std::fs::write(
        git_dir.join("refs/heads/kestrel/run-123456789abc"),
        format!("{NEW}\n"),
    )
    .unwrap();
    std::fs::write(git_dir.join("packed-refs"), "# pack-refs with: peeled\n").unwrap();
    s.fake
        .answer("ls-remote", 0, &format!("{NEW}\trefs/heads/{BRANCH_A}\n"));
    s.fake.answer("rev-parse", 1, "");

    let got = s.ws.bring_back(run_a(), &s.repo).await.unwrap();

    assert!(matches!(got, BroughtBack::Fetched { .. }), "{got:?}");
}

// ---------------------------------------------------------------------------------------------
// What git answers, scripted: every way a step can fail is an error, never a guess.
// ---------------------------------------------------------------------------------------------

const NEW: &str = "1111111111111111111111111111111111111111";
const BASE: &str = "2222222222222222222222222222222222222222";
const OTHER: &str = "3333333333333333333333333333333333333333";
const HOUR: Duration = Duration::from_secs(3600);

#[tokio::test]
async fn scripted_a_tip_the_source_holds_under_its_run_branch_is_already_fetched() {
    let s = Setup::new();
    s.fake
        .answer("ls-remote", 0, &format!("{NEW}\trefs/heads/{BRANCH_A}\n"));
    s.fake.answer("rev-parse", 0, &format!("{NEW}\n"));
    s.fake.answer(
        "for-each-ref",
        0,
        &format!("{BASE} refs/heads/main\n{NEW} refs/heads/{BRANCH_A}\n"),
    );

    let got = s.ws.bring_back(run_a(), &s.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::AlreadyFetched {
            branch: BRANCH_A.into(),
            commit: NEW.into(),
        }
    );
    assert!(!s.fake.ran("fetch"));
}

#[tokio::test]
async fn scripted_a_run_branch_the_user_moved_past_the_tip_is_already_fetched() {
    let s = Setup::new();
    s.fake
        .answer("ls-remote", 0, &format!("{NEW}\trefs/heads/{BRANCH_A}\n"));
    s.fake.answer("rev-parse", 0, &format!("{NEW}\n"));
    s.fake.answer(
        "for-each-ref",
        0,
        &format!("{BASE} refs/heads/main\n{OTHER} refs/heads/{BRANCH_A}\n"),
    );

    let got = s.ws.bring_back(run_a(), &s.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::AlreadyFetched {
            branch: BRANCH_A.into(),
            commit: NEW.into(),
        }
    );
    assert!(!s.fake.ran("fetch"));
}

#[tokio::test(start_paused = true)]
async fn scripted_a_hung_git_call_is_stopped_after_120_s_and_the_next_fetch_runs() {
    let s = Setup::new();
    s.fake
        .answer("ls-remote", 0, &format!("{NEW}\trefs/heads/{BRANCH_A}\n"));
    s.fake.answer("rev-parse", 1, "");
    s.fake.hang("fetch");
    let start = tokio::time::Instant::now();

    let first = tokio::time::timeout(HOUR, s.ws.bring_back(run_a(), &s.repo))
        .await
        .expect("a hung git call held bring_back for an hour");

    let err = first.unwrap_err();
    assert!(err.to_string().contains("fetch"), "{err}");
    assert_eq!(start.elapsed().as_secs(), 120, "{err}");
    // The lock is free again: the next run's fetch is not stuck behind the hung one.
    s.fake.hang("nothing");
    let second = tokio::time::timeout(HOUR, s.ws.bring_back(run_a(), &s.repo))
        .await
        .expect("the next fetch waited behind the hung one");
    assert!(
        matches!(second, Ok(BroughtBack::Fetched { .. })),
        "{second:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn scripted_the_git_timeout_can_be_changed() {
    let s = Setup::new();
    s.fake.hang("ls-remote");
    let ws = Workspaces::new(s.clone.parent().unwrap().to_path_buf(), s.fake.clone())
        .with_git_timeout(Duration::from_secs(5));
    let start = tokio::time::Instant::now();

    let got = tokio::time::timeout(HOUR, ws.bring_back(run_a(), &s.repo))
        .await
        .expect("the timeout was not applied");

    let err = got.unwrap_err();
    assert!(err.to_string().contains("ls-remote"), "{err}");
    assert_eq!(start.elapsed().as_secs(), 5, "{err}");
}

#[tokio::test]
async fn scripted_a_tip_reachable_from_another_ref_is_nothing_new() {
    let s = Setup::new();
    s.fake
        .answer("ls-remote", 0, &format!("{BASE}\trefs/heads/{BRANCH_A}\n"));
    s.fake.answer("rev-parse", 0, &format!("{BASE}\n"));
    s.fake
        .answer("for-each-ref", 0, &format!("{NEW} refs/heads/main\n"));

    let got = s.ws.bring_back(run_a(), &s.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::NothingNew {
            branch: BRANCH_A.into(),
            commit: BASE.into(),
        }
    );
    assert!(!s.fake.ran("fetch"));
}

#[tokio::test]
async fn scripted_a_tip_the_source_holds_but_no_ref_reaches_is_fetched() {
    let s = Setup::new();
    s.fake
        .answer("ls-remote", 0, &format!("{NEW}\trefs/heads/{BRANCH_A}\n"));
    s.fake.answer("rev-parse", 0, &format!("{NEW}\n"));
    s.fake.answer("for-each-ref", 0, "");

    let got = s.ws.bring_back(run_a(), &s.repo).await.unwrap();

    assert!(matches!(got, BroughtBack::Fetched { .. }), "{got:?}");
    assert!(s.fake.ran("fetch"));
}

#[tokio::test]
async fn scripted_ls_remote_lines_for_other_refs_mean_no_branch() {
    let s = Setup::new();
    // ls-remote matches patterns by their tail, so a longer ref name can come back too.
    s.fake.answer(
        "ls-remote",
        0,
        &format!("{NEW}\trefs/heads/x/refs/heads/{BRANCH_A}\n"),
    );

    let got = s.ws.bring_back(run_a(), &s.repo).await.unwrap();

    assert_eq!(
        got,
        BroughtBack::NoBranch {
            branch: BRANCH_A.into()
        }
    );
    assert_eq!(s.fake.argvs().len(), 1);
}

#[tokio::test]
async fn scripted_a_tip_that_is_not_a_commit_id_is_an_error() {
    let s = Setup::new();
    s.fake.answer(
        "ls-remote",
        0,
        &format!("--upload-pack=evil\trefs/heads/{BRANCH_A}\n"),
    );

    let err = s.ws.bring_back(run_a(), &s.repo).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::BadCommit { .. }), "{err:?}");
    assert!(!s.fake.ran("fetch"));
}

#[tokio::test]
async fn scripted_a_failing_step_is_a_git_error_and_nothing_is_fetched() {
    for (step, code) in [
        ("ls-remote", 128),
        ("rev-parse", 128),
        ("for-each-ref", 129),
    ] {
        let s = Setup::new();
        s.fake
            .answer("ls-remote", 0, &format!("{NEW}\trefs/heads/{BRANCH_A}\n"));
        s.fake.answer("rev-parse", 0, &format!("{NEW}\n"));
        s.fake.answer("for-each-ref", 0, "");
        s.fake.answer(step, code, "");

        let err = s.ws.bring_back(run_a(), &s.repo).await.unwrap_err();

        match &err {
            WorkspaceError::Git { argv, status, .. } => {
                assert_eq!(*status, code, "{step}");
                assert!(argv.iter().any(|a| a == step), "{step}: {argv:?}");
            }
            other => panic!("{step}: expected Git, got {other:?}"),
        }
        assert!(!s.fake.ran("fetch"), "{step}");
    }
}

#[tokio::test]
async fn scripted_a_failed_fetch_is_a_git_error() {
    let s = Setup::new();
    s.fake
        .answer("ls-remote", 0, &format!("{NEW}\trefs/heads/{BRANCH_A}\n"));
    s.fake.answer("rev-parse", 1, "");
    s.fake.answer("fetch", 1, "");

    let err = s.ws.bring_back(run_a(), &s.repo).await.unwrap_err();

    assert!(
        matches!(&err, WorkspaceError::Git { argv, .. } if argv.iter().any(|a| a == "fetch")),
        "{err:?}"
    );
}

#[tokio::test]
async fn scripted_git_that_cannot_start_is_an_io_error() {
    let s = Setup::new();
    s.fake.cannot_start();

    let err = s.ws.bring_back(run_a(), &s.repo).await.unwrap_err();

    assert!(matches!(err, WorkspaceError::Io { .. }), "{err:?}");
}

#[test]
fn brought_back_names_its_branch_and_commit() {
    let fetched = BroughtBack::Fetched {
        branch: BRANCH_A.into(),
        commit: NEW.into(),
    };
    assert_eq!(fetched.branch(), Some(BRANCH_A));
    assert_eq!(fetched.commit(), Some(NEW));
    let none = BroughtBack::NoClone;
    assert_eq!(none.branch(), None);
    assert_eq!(none.commit(), None);
    let missing = BroughtBack::NoBranch {
        branch: BRANCH_A.into(),
    };
    assert_eq!(missing.branch(), Some(BRANCH_A));
    assert_eq!(missing.commit(), None);
    for (value, name) in [
        (fetched, "fetched"),
        (
            BroughtBack::AlreadyFetched {
                branch: BRANCH_A.into(),
                commit: NEW.into(),
            },
            "already_fetched",
        ),
        (
            BroughtBack::NothingNew {
                branch: BRANCH_A.into(),
                commit: NEW.into(),
            },
            "nothing_new",
        ),
        (missing, "no_branch"),
        (none, "no_clone"),
    ] {
        assert_eq!(value.name(), name);
    }
}

// ---------------------------------------------------------------------------------------------
// Scripted runner

/// One recorded call: the argv and the environment it ran with.
type Call = (Vec<String>, Option<EnvScrub>);

/// Answers each git subcommand with a scripted `(status, stdout)`, `(0, "")` when unscripted.
/// The subcommand set with [`Scripted::hang`] never answers, like a stuck `upload-pack`.
#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<Call>>,
    answers: Mutex<Vec<(String, i32, String)>>,
    broken: Mutex<bool>,
    hangs: Mutex<String>,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn answer(&self, subcommand: &str, status: i32, stdout: &str) {
        let mut answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        answers.retain(|(name, _, _)| name != subcommand);
        answers.push((subcommand.into(), status, stdout.into()));
    }

    fn cannot_start(&self) {
        *self.broken.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }

    fn hang(&self, subcommand: &str) {
        *self.hangs.lock().unwrap_or_else(PoisonError::into_inner) = subcommand.into();
    }

    fn hangs(&self, argv: &[String]) -> bool {
        *self.hangs.lock().unwrap_or_else(PoisonError::into_inner) == subcommand(argv)
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn argvs(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().map(|(a, _)| a).collect()
    }

    fn ran(&self, name: &str) -> bool {
        self.argvs().iter().any(|a| subcommand(a) == name)
    }

    fn reply(&self, argv: &[String], env: Option<EnvScrub>) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((argv.to_vec(), env));
        if *self.broken.lock().unwrap_or_else(PoisonError::into_inner) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "git not found",
            ));
        }
        let name = subcommand(argv);
        let answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        let (status, stdout) = answers
            .iter()
            .find(|(n, _, _)| n == name)
            .map_or((0, String::new()), |(_, s, o)| (*s, o.clone()));
        Ok(CommandOutput {
            status,
            stdout,
            stderr: if status == 0 {
                String::new()
            } else {
                format!("fatal: {name} failed")
            },
        })
    }
}

/// The git subcommand of `argv`: the first word after the `-C <dir>` and `-c <pair>` options.
fn subcommand(argv: &[String]) -> &str {
    let mut i = 1;
    while i < argv.len() && (argv[i] == "-C" || argv[i] == "-c") {
        i += 2;
    }
    argv.get(i).map_or("", String::as_str)
}

#[async_trait]
impl CommandRunner for Scripted {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        self.reply(argv, None)
    }

    async fn run_with_env(
        &self,
        argv: &[String],
        env: &EnvScrub,
    ) -> std::io::Result<CommandOutput> {
        if self.hangs(argv) {
            return std::future::pending().await;
        }
        self.reply(argv, Some(env.clone()))
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        panic!("bring_back never spawns: {argv:?}");
    }
}

/// A clone directory that passes the `.git` checks, a source repo path, and a scripted runner.
struct Setup {
    _tmp: TempDir,
    repo: PathBuf,
    clone: PathBuf,
    fake: Arc<Scripted>,
    ws: Workspaces,
}

impl Setup {
    fn new() -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let clone = root.join(run_a().to_string());
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        std::fs::write(clone.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = tmp.path().join("source");
        let fake = Scripted::new();
        let ws = Workspaces::new(root, fake.clone());
        Setup {
            _tmp: tmp,
            repo,
            clone,
            fake,
            ws,
        }
    }
}
