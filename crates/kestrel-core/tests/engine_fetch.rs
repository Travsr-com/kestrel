//! Issue #93 at the engine: when a run with a `git-clone` workspace ends (completed, failed or
//! cancelled), its branch `kestrel/run-<id>` comes home into the source repository by itself, if
//! the clone has commits the source does not. A failed fetch never changes how the run ended; it
//! is recorded as a warning in the run's log.
//!
//! The fake driver and adapter from `common` stand in for the sandbox and the agent; the test
//! commits in the run's clone itself, as the agent would. Git is real and every test skips with a
//! message when it is not installed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use kestrel_core::Workspaces;
use kestrel_sandbox::{CommandOutput, CommandRunner, EnvScrub, ProcessRunner, SpawnedProcess};
use kestrel_store::ControlCommand;
use kestrel_types::{AgentSpec, EventKind, RunId, RunStatus, WorkspaceSpec};
use tempfile::TempDir;

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

/// Real git, recording every call once it has finished, so a test can tell that the engine's
/// `bring_back` is over even when it changed nothing.
#[derive(Default)]
struct Recording {
    finished: Mutex<Vec<Vec<String>>>,
}

impl Recording {
    fn done(&self, argv: &[String]) {
        self.finished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
    }

    /// Every finished call's argv.
    fn calls(&self) -> Vec<Vec<String>> {
        self.finished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether a finished call had `word` as one of its arguments.
    fn ran(&self, word: &str) -> bool {
        self.calls()
            .iter()
            .any(|argv| argv.iter().any(|a| a == word))
    }
}

#[async_trait]
impl CommandRunner for Recording {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        let out = ProcessRunner.run(argv).await;
        self.done(argv);
        out
    }

    async fn run_with_env(
        &self,
        argv: &[String],
        env: &EnvScrub,
    ) -> std::io::Result<CommandOutput> {
        let out = ProcessRunner.run_with_env(argv, env).await;
        self.done(argv);
        out
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        ProcessRunner.spawn(argv).await
    }
}

/// A source repo with one commit, the workspaces root, and an engine on real git.
struct Setup {
    _tmp: TempDir,
    repo: PathBuf,
    root: PathBuf,
    git: Arc<Recording>,
    h: Harness,
}

fn setup(max: usize) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("source");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    commit_file(&repo, "shared.txt", "base\n");
    let root = tmp.path().join("workspaces");
    let recording = Arc::new(Recording::default());
    let mut h = Harness::new(max);
    let ws_root = root.clone();
    let runner = recording.clone();
    h.rebuild(move |e| e.with_workspaces(Workspaces::new(ws_root, runner)));
    h.start();
    Setup {
        _tmp: tmp,
        repo,
        root,
        git: recording,
        h,
    }
}

fn clone_of(repo: &Path) -> AgentSpec {
    let mut s = spec();
    s.sandbox.workspace = WorkspaceSpec::GitClone {
        repo: repo.to_path_buf(),
        depth: None,
    };
    s
}

/// The commit `branch` is at in `repo`, `None` when there is no such branch.
fn branch_tip(repo: &Path, branch: &str) -> Option<String> {
    let out = git_raw(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    );
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn log_lines(h: &Harness, id: RunId) -> Vec<String> {
    h.events(id)
        .into_iter()
        .filter_map(|k| match k {
            EventKind::AgentLog { line } => Some(line),
            _ => None,
        })
        .collect()
}

fn fetched_line(h: &Harness, id: RunId) -> Option<String> {
    log_lines(h, id)
        .into_iter()
        .find(|l| l.starts_with("kestrel: fetched branch"))
}

fn warning(h: &Harness, id: RunId) -> Option<String> {
    log_lines(h, id)
        .into_iter()
        .find(|l| l.starts_with("kestrel: warning:"))
}

/// Real-time bound for every wait here: each one waits on real `git` processes (clones, commits,
/// fetches), which take far longer than the harness default on slow runners.
const SLOW_DEADLINE: Duration = Duration::from_secs(60);

/// [`until`] with [`SLOW_DEADLINE`].
async fn wait(what: &str, cond: impl FnMut() -> bool) {
    until_within(what, SLOW_DEADLINE, cond).await;
}

/// Waits until the run's log says how its branch came home, then asserts it was fetched.
async fn fetched(h: &Harness, id: RunId) {
    wait("fetch or warning recorded", || {
        fetched_line(h, id).is_some() || warning(h, id).is_some()
    })
    .await;
    assert_eq!(warning(h, id), None, "{:?}", log_lines(h, id));
}

/// Starts a run that stays alive until the test ends it, and waits until it runs. Returns the
/// run, its clone and the control of its agent.
async fn running(s: &Setup) -> (RunId, PathBuf, SessionCtl) {
    let ctl = s.h.driver.live();
    let id = s.h.submit(clone_of(&s.repo)).await;
    wait("run running or ended", || {
        let status = s.h.status(id);
        status == RunStatus::Running || status.is_terminal()
    })
    .await;
    assert_eq!(
        s.h.status(id),
        RunStatus::Running,
        "{:?}",
        log_lines(&s.h, id)
    );
    (id, s.root.join(id.to_string()), ctl)
}

// Scenario 1 ------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s1_a_completed_run_with_a_commit_brings_its_branch_home() {
    need_git!();
    let s = setup(4);
    let (id, clone, ctl) = running(&s).await;
    let tip = commit_file(&clone, "agent.txt", "work\n");
    let branch = Workspaces::branch_for(id);
    assert_eq!(branch_tip(&s.repo, &branch), None, "nothing before the end");

    ctl.line("ok:done");
    ctl.exit(0);
    wait("run completed", || s.h.is(id, &RunStatus::Completed)).await;
    fetched(&s.h, id).await;

    assert_eq!(branch_tip(&s.repo, &branch), Some(tip.clone()));
    let line = fetched_line(&s.h, id).unwrap();
    assert!(line.contains(&branch), "{line}");
    assert!(line.contains(&tip[..12]), "{line}");
    assert_eq!(warning(&s.h, id), None);
    assert!(s.h.is(id, &RunStatus::Completed));
    // The user's checkout did not move.
    assert_eq!(git(&s.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(git(&s.repo, &["status", "--porcelain"]), "");
}

/// A host git template (`init.templateDir`) may hold links, say hooks linked from a dotfiles
/// repo. Copied into the clone, any link in `.git` makes the fetch refuse the clone, so the clone
/// gets no template at all and its branch still comes home.
#[tokio::test(start_paused = true)]
async fn s1_the_clone_gets_no_git_template_and_its_branch_still_comes_home() {
    need_git!();
    let s = setup(4);
    let (id, clone, ctl) = running(&s).await;
    for from_template in ["hooks", "description", "info/exclude"] {
        assert!(
            !clone.join(".git").join(from_template).exists(),
            "`.git/{from_template}` came from the host's git template"
        );
    }
    let tip = commit_file(&clone, "agent.txt", "work\n");

    ctl.line("ok:done");
    ctl.exit(0);
    wait("run completed", || s.h.is(id, &RunStatus::Completed)).await;
    fetched(&s.h, id).await;

    let branch = Workspaces::branch_for(id);
    assert_eq!(branch_tip(&s.repo, &branch), Some(tip));
    assert_eq!(warning(&s.h, id), None);
}

#[tokio::test(start_paused = true)]
async fn s1_a_failed_run_with_a_commit_brings_its_branch_home_and_stays_failed() {
    need_git!();
    let s = setup(4);
    let (id, clone, ctl) = running(&s).await;
    let tip = commit_file(&clone, "agent.txt", "half done\n");

    ctl.line("fail:gave up");
    ctl.exit(1);
    wait("run failed", || s.h.is_failed(id).is_some()).await;
    fetched(&s.h, id).await;

    assert_eq!(branch_tip(&s.repo, &Workspaces::branch_for(id)), Some(tip));
    assert_eq!(s.h.is_failed(id).as_deref(), Some("gave up"));
}

#[tokio::test(start_paused = true)]
async fn s1_a_cancelled_run_with_a_commit_brings_its_branch_home() {
    need_git!();
    let s = setup(4);
    let (id, clone, _ctl) = running(&s).await;
    let tip = commit_file(&clone, "agent.txt", "partial\n");

    s.h.control(id, ControlCommand::Cancel);
    wait("run cancelled", || s.h.is(id, &RunStatus::Cancelled)).await;
    fetched(&s.h, id).await;

    assert_eq!(branch_tip(&s.repo, &Workspaces::branch_for(id)), Some(tip));
    assert!(s.h.is(id, &RunStatus::Cancelled));
}

// Scenario 2 ------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s2_six_runs_editing_the_same_file_arrive_as_six_branches() {
    need_git!();
    let s = setup(6);
    let main = git(&s.repo, &["rev-parse", "main"]);
    let mut runs = Vec::new();
    for _ in 0..6 {
        runs.push(running(&s).await);
    }
    let mut tips = Vec::new();
    for (n, (_, clone, _)) in runs.iter().enumerate() {
        tips.push(commit_file(clone, "shared.txt", &format!("kid {n}\n")));
    }

    let mut ids = Vec::new();
    for (id, _, ctl) in runs {
        ctl.line("ok:done");
        ctl.exit(0);
        ids.push(id);
    }
    wait("all fetched", || {
        ids.iter().all(|id| fetched_line(&s.h, *id).is_some())
    })
    .await;

    for (n, id) in ids.iter().enumerate() {
        let branch = Workspaces::branch_for(*id);
        assert_eq!(branch_tip(&s.repo, &branch), Some(tips[n].clone()));
        assert_eq!(
            git(&s.repo, &["show", &format!("{branch}:shared.txt")]),
            format!("kid {n}")
        );
        assert_eq!(git(&s.repo, &["rev-parse", &format!("{branch}^")]), main);
        assert!(s.h.is(*id, &RunStatus::Completed));
    }
    let heads = git(&s.repo, &["branch", "--format=%(refname:short)"]);
    assert_eq!(
        heads.lines().count(),
        7,
        "main and six run branches: {heads}"
    );
    assert_eq!(git(&s.repo, &["rev-parse", "main"]), main);
    assert_eq!(git(&s.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(
        std::fs::read_to_string(s.repo.join("shared.txt")).unwrap(),
        "base\n"
    );
}

// Scenario 3 ------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s3_a_run_without_new_commits_fetches_nothing_and_logs_nothing() {
    need_git!();
    let s = setup(4);
    let (id, clone, ctl) = running(&s).await;
    std::fs::write(clone.join("draft.txt"), "never committed\n").unwrap();
    let before = git(&s.repo, &["for-each-ref"]);

    ctl.line("ok:done");
    ctl.exit(0);
    wait("run completed", || s.h.is(id, &RunStatus::Completed)).await;
    // `bring_back` ends with `for-each-ref --contains` when the source already has the tip.
    wait("bring_back looked and finished", || {
        s.git.ran("for-each-ref")
    })
    .await;
    settle().await;

    assert!(!s.git.ran("fetch"), "a fetch ran: {:?}", s.git.calls());
    assert_eq!(git(&s.repo, &["for-each-ref"]), before);
    assert_eq!(fetched_line(&s.h, id), None);
    assert_eq!(warning(&s.h, id), None);
}

// Scenario 4 ------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn s4_the_users_dirty_tree_and_new_commits_are_left_alone() {
    need_git!();
    let s = setup(4);
    let (id, clone, ctl) = running(&s).await;
    let tip = commit_file(&clone, "shared.txt", "agent\n");
    let mine = commit_file(&s.repo, "user.txt", "mine\n");
    std::fs::write(s.repo.join("shared.txt"), "user edit\n").unwrap();
    let status = git(&s.repo, &["status", "--porcelain"]);

    ctl.line("ok:done");
    ctl.exit(0);
    fetched(&s.h, id).await;

    assert_eq!(branch_tip(&s.repo, &Workspaces::branch_for(id)), Some(tip));
    assert_eq!(git(&s.repo, &["rev-parse", "main"]), mine);
    assert_eq!(git(&s.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(git(&s.repo, &["status", "--porcelain"]), status);
    assert_eq!(
        std::fs::read_to_string(s.repo.join("shared.txt")).unwrap(),
        "user edit\n"
    );
}

// Scenario 5 and failures -----------------------------------------------------------------------

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
    let tree_file = clone.join("..").join("bad-tree.bin");
    std::fs::write(&tree_file, [entry.clone(), entry].concat()).unwrap();
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
}

#[tokio::test(start_paused = true)]
async fn s5_an_object_that_fails_fsck_is_refused_with_a_warning_and_the_run_still_completes() {
    need_git!();
    let s = setup(4);
    let (id, clone, ctl) = running(&s).await;
    plant_bad_commit(&clone, &Workspaces::branch_for(id));
    let before = git(&s.repo, &["for-each-ref"]);

    ctl.line("ok:done");
    ctl.exit(0);
    wait("warning recorded", || warning(&s.h, id).is_some()).await;

    assert!(s.h.is(id, &RunStatus::Completed));
    assert_eq!(git(&s.repo, &["for-each-ref"]), before, "no ref arrived");
    let line = warning(&s.h, id).unwrap();
    assert!(line.contains(&Workspaces::branch_for(id)), "{line}");
    assert!(line.contains(&format!("kestrel fetch {id}")), "{line}");
    assert_eq!(fetched_line(&s.h, id), None);
}

#[tokio::test(start_paused = true)]
async fn a_branch_that_cannot_fast_forward_is_a_warning_and_the_users_branch_is_kept() {
    need_git!();
    let s = setup(4);
    let (id, clone, ctl) = running(&s).await;
    commit_file(&clone, "agent.txt", "work\n");
    // The user already made a branch of that name, at a commit the clone does not build on.
    let branch = Workspaces::branch_for(id);
    git(&s.repo, &["checkout", "-q", "-b", "scratch"]);
    let theirs = commit_file(&s.repo, "their.txt", "theirs\n");
    git(&s.repo, &["branch", "-q", &branch, &theirs]);
    git(&s.repo, &["checkout", "-q", "main"]);

    ctl.line("ok:done");
    ctl.exit(0);
    wait("warning recorded", || warning(&s.h, id).is_some()).await;

    assert!(s.h.is(id, &RunStatus::Completed));
    assert_eq!(branch_tip(&s.repo, &branch), Some(theirs));
}
