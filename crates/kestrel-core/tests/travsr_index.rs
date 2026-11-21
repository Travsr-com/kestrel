//! Issue #44: the engine has each run's code indexed for Travsr before its agent starts, and
//! forgotten when the run ends, through a fake `RunIndexer`.
//!
//! Travsr runs on the host, so the indexer is never given the run's workspace: it gets the source
//! repo and the commit the workspace was cloned at (read before the sandbox exists), and keeps its
//! own copy.
//!
//! Every test runs on a paused clock with the fake driver and adapter from `common`, and a fake
//! git that makes a clone directory and answers `rev-parse` with a commit id. Test names start
//! with the scenario number of the issue they cover (`s1_...`); rules the engine adds on top have
//! no number.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use kestrel_core::caps::RootKey;
use kestrel_core::{IndexLimits, IndexSource, RunIndexer, Workspaces};
use kestrel_sandbox::{CommandOutput, CommandRunner, SpawnedProcess};
use kestrel_store::ControlCommand;
use kestrel_types::{AgentSpec, EventKind, RunId, RunStatus, WorkspaceSpec};
use tempfile::TempDir;
use tokio::sync::Semaphore;

use RunStatus::{Completed, Running, Suspended};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Git that succeeds: `clone` makes the target directory with a `.git` entry, and `rev-parse
/// HEAD^{commit}` answers [`SHA`] (or fails, when `head_fails`). Records every argv.
#[derive(Default)]
struct FakeGit {
    head_fails: AtomicBool,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeGit {
    /// Calls that ran inside `dir` (`git -C <dir> ...`).
    fn inside(&self, dir: &Path) -> Vec<Vec<String>> {
        let dir = dir.to_string_lossy();
        lock(&self.calls)
            .iter()
            .filter(|argv| argv.get(1).is_some_and(|a| a == "-C") && argv[2] == dir)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl CommandRunner for FakeGit {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        lock(&self.calls).push(argv.to_vec());
        if argv.get(1).is_some_and(|a| a == "clone") {
            let path = PathBuf::from(argv.last().expect("clone target"));
            std::fs::create_dir_all(path.join(".git"))?;
        }
        if argv.iter().any(|a| a == "HEAD^{commit}") && self.head_fails.load(Ordering::SeqCst) {
            return Ok(CommandOutput {
                status: 128,
                stdout: String::new(),
                stderr: "fatal: Needed a single revision".into(),
            });
        }
        let stdout = if argv.iter().any(|a| a == "HEAD^{commit}") {
            format!("{SHA}\n")
        } else {
            ".git\n".into()
        };
        Ok(CommandOutput {
            status: 0,
            stdout,
            stderr: String::new(),
        })
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        panic!("workspaces never spawn: {argv:?}");
    }
}

/// A fake Travsr indexer: records every call, and can wait, fail or take time.
#[derive(Default)]
struct FakeIndexer {
    indexed: Mutex<Vec<(RunId, IndexSource)>>,
    released: Mutex<Vec<RunId>>,
    forgotten: Mutex<Vec<RunId>>,
    /// The runs it keeps a copy for: indexed and not forgotten yet.
    disk: Mutex<HashSet<RunId>>,
    /// When set, `index` waits for a permit of it (one per call).
    gate: Mutex<Option<Arc<Semaphore>>>,
    /// When set, `forget` waits for a permit of it (one per call).
    forget_gate: Mutex<Option<Arc<Semaphore>>>,
    /// How long `index` takes.
    takes: Mutex<Duration>,
    /// When set, `index` fails with this.
    fails: Mutex<Option<String>>,
    /// How many of the next `forget` calls fail.
    forget_failures: AtomicUsize,
    /// `kept` fails.
    kept_fails: AtomicBool,
    running: AtomicUsize,
    most_running: AtomicUsize,
    finished: AtomicUsize,
}

impl FakeIndexer {
    fn indexed(&self) -> Vec<(RunId, IndexSource)> {
        lock(&self.indexed).clone()
    }

    fn forgotten(&self) -> Vec<RunId> {
        lock(&self.forgotten).clone()
    }

    fn released(&self) -> Vec<RunId> {
        lock(&self.released).clone()
    }

    /// Makes `index` wait until a permit is added.
    fn close(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *lock(&self.gate) = Some(gate.clone());
        gate
    }
}

/// Takes one from `left`, if any are left. A loop rather than `fetch_update`, which newer
/// toolchains deprecate, and its replacement is past the MSRV.
fn take_one(left: &AtomicUsize) -> bool {
    let mut now = left.load(Ordering::SeqCst);
    loop {
        if now == 0 {
            return false;
        }
        match left.compare_exchange(now, now - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(seen) => now = seen,
        }
    }
}

#[async_trait]
impl RunIndexer for FakeIndexer {
    async fn index(&self, run: RunId, source: &IndexSource) -> Result<(), String> {
        lock(&self.indexed).push((run, source.clone()));
        lock(&self.disk).insert(run);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_running.fetch_max(now, Ordering::SeqCst);
        // Counted down however this ends, a timeout included.
        struct Done<'a>(&'a FakeIndexer);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.running.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _done = Done(self);
        let gate = lock(&self.gate).clone();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        let takes = *lock(&self.takes);
        tokio::time::sleep(takes).await;
        self.finished.fetch_add(1, Ordering::SeqCst);
        match lock(&self.fails).clone() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    async fn release(&self, run: RunId) {
        lock(&self.released).push(run);
    }

    async fn forget(&self, run: RunId) -> Result<(), String> {
        lock(&self.forgotten).push(run);
        let gate = lock(&self.forget_gate).clone();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        if take_one(&self.forget_failures) {
            return Err("could not delete the copy: permission denied".into());
        }
        lock(&self.disk).remove(&run);
        Ok(())
    }

    async fn kept(&self) -> Result<Vec<RunId>, String> {
        if self.kept_fails.load(Ordering::SeqCst) {
            return Err("permission denied".into());
        }
        Ok(lock(&self.disk).iter().copied().collect())
    }
}

const LIMITS: IndexLimits = IndexLimits {
    timeout: Duration::from_secs(5),
    max_jobs: 2,
    retry_every: Duration::from_secs(10),
    forget_timeout: Duration::from_secs(3),
};

struct Setup {
    h: Harness,
    ix: Arc<FakeIndexer>,
    git: Arc<FakeGit>,
    /// The source repo every clone is made from, and the workspaces root.
    repo: PathBuf,
    root: PathBuf,
    _dir: TempDir,
}

impl Setup {
    /// A new engine on the same database and workspaces, with a new indexer and git, as after
    /// a restart. Call after the old engine stopped.
    fn restart(&mut self, ix: Arc<FakeIndexer>) {
        let (root, indexer, git) = (self.root.clone(), ix.clone(), self.git.clone());
        self.h.shutdown = tokio_util::sync::CancellationToken::new();
        self.h.rebuild(move |e| {
            e.with_workspaces(Workspaces::new(root, git))
                .with_root_key(RootKey::generate())
                .with_indexer(indexer, LIMITS)
        });
        self.ix = ix;
    }
}

/// A harness whose engine has workspaces (fake git), the fake indexer with `limits` and a root key.
fn setup(max: usize, limits: IndexLimits) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let root = dir.path().join("workspaces");
    std::fs::create_dir_all(&repo).unwrap();
    let mut h = Harness::new(max);
    let ix = Arc::new(FakeIndexer::default());
    let git = Arc::new(FakeGit::default());
    let (ws_root, indexer, runner) = (root.clone(), ix.clone(), git.clone());
    h.rebuild(move |e| {
        e.with_workspaces(Workspaces::new(ws_root, runner))
            .with_root_key(RootKey::generate())
            .with_indexer(indexer, limits)
    });
    Setup {
        h,
        ix,
        git,
        repo,
        root,
        _dir: dir,
    }
}

/// A spec with these capability tools and a clone of `repo`.
fn clone_spec(repo: &Path, mcp: &[&str]) -> AgentSpec {
    let mut s = spec();
    s.capabilities.mcp = mcp.iter().map(|t| (*t).to_string()).collect();
    s.sandbox.workspace = WorkspaceSpec::GitClone {
        repo: repo.to_path_buf(),
        depth: None,
    };
    s
}

fn travsr_logs(h: &Harness, id: RunId) -> Vec<String> {
    h.events(id)
        .into_iter()
        .filter_map(|k| match k {
            EventKind::AgentLog { line } if line.contains("Travsr") => Some(line),
            _ => None,
        })
        .collect()
}

fn at_start(repo: &Path) -> IndexSource {
    IndexSource {
        repo: repo.to_path_buf(),
        commit: Some(SHA.to_string()),
    }
}

#[tokio::test(start_paused = true)]
async fn s1_the_run_s_code_is_indexed_from_its_source_at_its_commit_before_the_agent_starts() {
    let mut s = setup(4, LIMITS);
    let gate = s.ix.close();
    let session = s.h.driver.live();
    s.h.start();
    let id =
        s.h.submit(clone_spec(
            &s.repo,
            &["travsr.get_callers", "travsr.search_symbol"],
        ))
        .await;
    until("indexing started", || s.ix.indexed().len() == 1).await;
    // The source repo and the commit its workspace was cloned at: never the workspace.
    assert_eq!(s.ix.indexed(), [(id, at_start(&s.repo))]);
    let workspace = s.root.join(id.to_string());
    let read = s.git.inside(&workspace);
    assert_eq!(read.len(), 2, "the checkout and the commit read: {read:?}");
    assert!(read[1].iter().any(|a| a == "rev-parse"), "{read:?}");
    // The sandbox may be made meanwhile, but the agent waits for the index.
    settle().await;
    assert_eq!(s.h.driver.starts(), 0);
    assert!(!s.h.is(id, &Running));
    assert_eq!(s.h.engine.travsr_tools(id).unwrap(), Some(Vec::new()));
    assert!(!s.h.engine.travsr_ready(id));

    gate.add_permits(1);
    until("running", || s.h.is(id, &Running)).await;
    assert_eq!(s.h.driver.starts(), 1);
    assert_eq!(
        s.h.engine.travsr_tools(id).unwrap(),
        Some(vec!["get_callers".to_string(), "search_symbol".to_string()])
    );
    assert!(s.h.engine.travsr_ready(id));
    assert!(
        travsr_logs(&s.h, id).is_empty(),
        "{:?}",
        travsr_logs(&s.h, id)
    );
    // No git ran in the workspace once its sandbox existed.
    assert_eq!(s.git.inside(&workspace).len(), 2);

    // Scenario 8: the run finishes, its copy is forgotten.
    session.line("ok:done");
    session.exit(0);
    until("completed", || s.h.is(id, &Completed)).await;
    until("forgotten", || !s.ix.forgotten().is_empty()).await;
    assert_eq!(s.ix.forgotten(), [id]);
    assert!(s.ix.released().is_empty());
    assert_eq!(s.h.engine.travsr_tools(id).unwrap(), None);
    assert!(!s.h.engine.travsr_ready(id));
}

#[tokio::test(start_paused = true)]
async fn s2_indexing_past_the_timeout_starts_the_run_without_travsr() {
    let mut s = setup(4, LIMITS);
    let _gate = s.ix.close();
    let _session = s.h.driver.live();
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("running", || s.h.is(id, &Running)).await;
    let logs = travsr_logs(&s.h, id);
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(
        logs[0].contains("Travsr is unavailable for this run") && logs[0].contains("within 5 s"),
        "{logs:?}"
    );
    assert_eq!(s.h.engine.travsr_tools(id).unwrap(), Some(Vec::new()));
    assert!(!s.h.engine.travsr_ready(id));
    // The job was given up: nothing is left indexing.
    assert_eq!(s.ix.running.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn s2_an_indexing_error_starts_the_run_without_travsr_saying_why() {
    let mut s = setup(4, LIMITS);
    *lock(&s.ix.fails) = Some("travsr init exited with code 2: no space left".into());
    let _session = s.h.driver.live();
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("running", || s.h.is(id, &Running)).await;
    let logs = travsr_logs(&s.h, id);
    assert!(
        logs.len() == 1 && logs[0].contains("no space left"),
        "{logs:?}"
    );
    assert_eq!(s.h.engine.travsr_tools(id).unwrap(), Some(Vec::new()));
}

#[tokio::test(start_paused = true)]
async fn s2_a_workspace_whose_commit_cannot_be_read_starts_without_travsr() {
    let mut s = setup(4, LIMITS);
    s.git.head_fails.store(true, Ordering::SeqCst);
    let _session = s.h.driver.live();
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("running", || s.h.is(id, &Running)).await;
    let logs = travsr_logs(&s.h, id);
    assert!(
        logs.len() == 1 && logs[0].contains("Needed a single revision"),
        "{logs:?}"
    );
    assert!(s.ix.indexed().is_empty());
    assert_eq!(s.h.engine.travsr_tools(id).unwrap(), Some(Vec::new()));
}

#[tokio::test(start_paused = true)]
async fn s8_a_failed_forget_is_logged_retried_and_never_fails_the_run() {
    let mut s = setup(4, LIMITS);
    s.ix.forget_failures.store(1, Ordering::SeqCst);
    s.h.driver.script(&["ok:done"], 0);
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("completed", || s.h.is(id, &Completed)).await;
    until("a forget", || s.ix.forgotten().len() == 1).await;
    until("logged", || !travsr_logs(&s.h, id).is_empty()).await;
    let logs = travsr_logs(&s.h, id);
    assert!(
        logs[0].contains("could not forget") && logs[0].contains("retried"),
        "{logs:?}"
    );
    assert_eq!(s.h.status(id), Completed);
    // Retried after `retry_every`, once: it works the second time.
    until("retried", || s.ix.forgotten().len() == 2).await;
    assert_eq!(s.ix.forgotten(), [id, id]);
    settle_for(400).await;
    assert_eq!(s.ix.forgotten().len(), 2);
    assert_eq!(travsr_logs(&s.h, id).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn s8_a_forget_that_keeps_failing_is_retried_again() {
    let mut s = setup(4, LIMITS);
    s.ix.forget_failures.store(3, Ordering::SeqCst);
    s.h.driver.script(&["ok:done"], 0);
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("completed", || s.h.is(id, &Completed)).await;
    // Each `until` may move the clock 20 s: two retries at most per wait.
    until("retried once", || s.ix.forgotten().len() >= 2).await;
    until("retried until it worked", || s.ix.forgotten().len() == 4).await;
    settle_for(400).await;
    assert_eq!(s.ix.forgotten().len(), 4);
    assert_eq!(s.h.status(id), Completed);
}

#[tokio::test(start_paused = true)]
async fn s8_a_forget_that_hangs_never_holds_the_run_s_slot() {
    // One slot: the second run starts only once the first gave it back. The forget timeout is
    // far away, so only a slot given back before the forget lets the second run start.
    let limits = IndexLimits {
        forget_timeout: Duration::from_secs(3600),
        ..LIMITS
    };
    let mut s = setup(1, limits);
    let stuck = Arc::new(Semaphore::new(0));
    *lock(&s.ix.forget_gate) = Some(stuck.clone());
    s.h.driver.script(&["ok:done"], 0);
    let _second_session = s.h.driver.live();
    s.h.start();
    let first =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    let second = s.h.submit(clone_spec(&s.repo, &["kestrel.join"])).await;
    until("first completed", || s.h.is(first, &Completed)).await;
    until("its forget began", || s.ix.forgotten().len() == 1).await;
    until("second running", || s.h.is(second, &Running)).await;
    assert!(
        travsr_logs(&s.h, first).is_empty(),
        "the forget is still under way"
    );
    stuck.add_permits(1);
    until("forgotten", || !lock(&s.ix.disk).contains(&first)).await;
}

#[tokio::test(start_paused = true)]
async fn s8_a_forget_that_hangs_is_given_up_logged_and_retried() {
    let mut s = setup(4, LIMITS);
    let stuck = Arc::new(Semaphore::new(0));
    *lock(&s.ix.forget_gate) = Some(stuck.clone());
    s.h.driver.script(&["ok:done"], 0);
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("completed", || s.h.is(id, &Completed)).await;
    // Given up after `forget_timeout` (3 s), logged, and retried later.
    until("logged", || !travsr_logs(&s.h, id).is_empty()).await;
    let logs = travsr_logs(&s.h, id);
    assert!(logs[0].contains("within 3 s"), "{logs:?}");
    stuck.add_permits(100);
    until("retried", || s.ix.forgotten().len() >= 2).await;
    until("forgotten", || !lock(&s.ix.disk).contains(&id)).await;
}

#[tokio::test(start_paused = true)]
async fn recovery_says_when_the_kept_copies_cannot_be_listed() {
    let mut s = setup(4, LIMITS);
    let ix = Arc::new(FakeIndexer::default());
    ix.kept_fails.store(true, Ordering::SeqCst);
    s.restart(ix);
    let report = s.h.engine.recover().await.unwrap();
    assert_eq!(report.travsr_unlisted.as_deref(), Some("permission denied"));
    // Without an indexer there is nothing to list.
    let mut h = Harness::new(1);
    h.rebuild(|e| e);
    assert_eq!(h.engine.recover().await.unwrap().travsr_unlisted, None);
}

#[tokio::test(start_paused = true)]
async fn s10_ten_children_are_indexed_at_most_two_at_a_time_and_all_get_travsr() {
    let mut s = setup(16, LIMITS);
    *lock(&s.ix.takes) = Duration::from_millis(500);
    let _lead_session = s.h.driver.live();
    let sessions: Vec<_> = (0..10).map(|_| s.h.driver.live()).collect();
    s.h.start();
    let mut lead = clone_spec(
        &s.repo,
        &[
            "kestrel.spawn",
            "travsr.get_callers",
            "travsr.search_symbol",
        ],
    );
    lead.capabilities.spawn = Some(kestrel_types::SpawnPolicy {
        max_children: 10,
        max_depth: 1,
        agents: vec!["worker".into()],
        keep_children: false,
    });
    lead.name = "lead".into();
    let lead_id = s.h.submit(lead).await;
    until("lead running", || s.h.is(lead_id, &Running)).await;
    let mut worker = clone_spec(Path::new("."), &["travsr.get_callers"]);
    worker.name = "worker".into();
    let mut children = Vec::new();
    for _ in 0..10 {
        let child =
            s.h.engine
                .spawn_child(lead_id, &worker, "look around", None)
                .await
                .unwrap();
        children.push(child);
    }
    until("every child running", || {
        children.iter().all(|c| s.h.is(*c, &Running))
    })
    .await;
    assert_eq!(s.ix.most_running.load(Ordering::SeqCst), 2);
    assert_eq!(s.ix.finished.load(Ordering::SeqCst), 11);
    let indexed: HashMap<RunId, IndexSource> = s.ix.indexed().into_iter().collect();
    for child in &children {
        // Each child on its own, from its parent's source repo at its own clone's commit.
        assert_eq!(indexed[child], at_start(&s.repo));
        assert_eq!(
            s.h.engine.travsr_tools(*child).unwrap(),
            Some(vec!["get_callers".to_string()])
        );
        assert!(travsr_logs(&s.h, *child).is_empty());
    }
    drop(sessions);
}

#[tokio::test(start_paused = true)]
async fn an_empty_dir_or_a_run_without_travsr_tools_is_not_indexed() {
    let mut s = setup(4, LIMITS);
    let _a = s.h.driver.live();
    let _b = s.h.driver.live();
    s.h.start();
    let mut empty = clone_spec(&s.repo, &["travsr.get_callers"]);
    empty.sandbox.workspace = WorkspaceSpec::EmptyDir;
    let empty = s.h.submit(empty).await;
    let no_tools = s.h.submit(clone_spec(&s.repo, &["kestrel.join"])).await;
    until("both running", || {
        s.h.is(empty, &Running) && s.h.is(no_tools, &Running)
    })
    .await;
    assert!(s.ix.indexed().is_empty());
    assert_eq!(s.h.engine.travsr_tools(empty).unwrap(), Some(Vec::new()));
    assert_eq!(s.h.engine.travsr_tools(no_tools).unwrap(), Some(Vec::new()));
    // Nor is the commit of their workspace read.
    assert!(s.git.inside(&s.root.join(no_tools.to_string())).len() == 1);
}

#[tokio::test(start_paused = true)]
async fn a_run_never_handed_to_the_indexer_is_never_forgotten() {
    // Finished runs without Travsr tools, or without a clone, never reach the indexer: not when
    // indexing, and not when they end (a missing travsr would otherwise fail every forget).
    let mut s = setup(4, LIMITS);
    s.h.driver.script(&["ok:done"], 0);
    s.h.driver.script(&["ok:done"], 0);
    s.h.start();
    let no_tools = s.h.submit(clone_spec(&s.repo, &["kestrel.join"])).await;
    let mut empty = clone_spec(&s.repo, &["travsr.get_callers"]);
    empty.sandbox.workspace = WorkspaceSpec::EmptyDir;
    let empty = s.h.submit(empty).await;
    until("both completed", || {
        s.h.is(no_tools, &Completed) && s.h.is(empty, &Completed)
    })
    .await;
    settle_for(400).await;
    assert!(s.ix.indexed().is_empty());
    assert!(s.ix.forgotten().is_empty(), "{:?}", s.ix.forgotten());
    assert!(s.ix.released().is_empty());
    assert!(travsr_logs(&s.h, no_tools).is_empty());
    assert!(travsr_logs(&s.h, empty).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_queued_run_starts_without_travsr_once_the_timeout_passes_however_long_it_queued() {
    // One job slot, held by a run whose index never finishes. The run behind it waits for the
    // slot and then for its own index, but both together at most the 5 s timeout: it starts
    // about 5 s after it began to wait, never 5 s after the slot came free.
    let limits = IndexLimits {
        max_jobs: 1,
        ..LIMITS
    };
    let mut s = setup(4, limits);
    let _gate = s.ix.close();
    let _first_session = s.h.driver.live();
    let _second_session = s.h.driver.live();
    s.h.start();
    let first =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("first indexing", || s.ix.indexed().len() == 1).await;
    // Most of the first run's timeout is gone before the second one starts to wait.
    tokio::time::advance(Duration::from_secs(3)).await;
    let waited_from = tokio::time::Instant::now();
    let second =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("second running", || s.h.is(second, &Running)).await;
    let waited = waited_from.elapsed();
    assert!(
        waited >= Duration::from_secs(5) && waited < Duration::from_secs(6),
        "{waited:?}"
    );
    let logs = travsr_logs(&s.h, second);
    assert!(
        logs.len() == 1 && logs[0].contains("within 5 s"),
        "{logs:?}"
    );
    assert_eq!(s.h.engine.travsr_tools(second).unwrap(), Some(Vec::new()));
    assert!(s.h.is(first, &Running));
}

#[tokio::test(start_paused = true)]
async fn without_an_indexer_nothing_is_indexed_and_no_travsr_tool_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut h = Harness::new(4);
    let root = dir.path().join("workspaces");
    h.rebuild(move |e| e.with_workspaces(Workspaces::new(root, Arc::new(FakeGit::default()))));
    let _session = h.driver.live();
    h.start();
    let id = h.submit(clone_spec(&repo, &["travsr.get_callers"])).await;
    until("running", || h.is(id, &Running)).await;
    assert_eq!(h.engine.travsr_tools(id).unwrap(), Some(Vec::new()));
    assert!(!h.engine.travsr_ready(id));
    assert_eq!(h.engine.travsr_tools(RunId::new()).unwrap(), None);
}

#[tokio::test(start_paused = true)]
async fn a_run_cancelled_while_indexing_never_starts_and_its_copy_is_forgotten() {
    let mut s = setup(4, LIMITS);
    let _gate = s.ix.close();
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("indexing started", || s.ix.indexed().len() == 1).await;
    s.h.control(id, ControlCommand::Cancel);
    until("cancelled", || s.h.status(id).is_terminal()).await;
    assert_eq!(s.h.status(id), RunStatus::Cancelled);
    assert_eq!(s.h.driver.starts(), 0);
    until("forgotten", || s.ix.forgotten().len() == 1).await;
    assert_eq!(s.ix.forgotten(), [id]);
    assert!(s.h.driver.leaked().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_run_resumed_after_a_restart_keeps_its_copy_and_is_never_indexed_from_its_workspace() {
    let mut s = setup(4, LIMITS);
    let _first = s.h.driver.live();
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("running", || s.h.is(id, &Running)).await;
    // The engine stops without cleaning up.
    s.h.shutdown.cancel();
    s.h.task.take().unwrap().await.unwrap().unwrap();
    let workspace = s.root.join(id.to_string());
    let git_before = s.git.inside(&workspace).len();

    // A new engine on the same database, workspaces and Travsr copies.
    let ix = Arc::new(FakeIndexer::default());
    lock(&ix.disk).insert(id);
    s.restart(ix);
    s.h.engine.recover().await.unwrap();
    assert!(matches!(s.h.status(id), Suspended(_)));
    // Unfinished: its copy is kept for the resume.
    assert!(s.ix.forgotten().is_empty(), "{:?}", s.ix.forgotten());
    let _second = s.h.driver.live();
    s.h.start();
    s.h.control(id, ControlCommand::Resume);
    until("running again", || s.h.is(id, &Running)).await;
    // Indexed again from its source, with no commit: the indexer uses the one it recorded.
    let resumed = IndexSource {
        repo: s.repo.clone(),
        commit: None,
    };
    assert_eq!(s.ix.indexed(), [(id, resumed)]);
    assert!(s.h.engine.travsr_ready(id));
    // No git ran in the workspace the agent had.
    assert_eq!(s.git.inside(&workspace).len(), git_before);
}

#[tokio::test(start_paused = true)]
async fn recovery_forgets_the_copies_of_finished_and_unknown_runs_and_keeps_the_others() {
    let mut s = setup(4, LIMITS);
    let store = s.h.store.clone();
    let spec = clone_spec(&s.repo, &["travsr.get_callers"]);
    let step = |id: RunId, from: RunStatus, to: RunStatus| store.transition(id, &from, to).unwrap();
    let done = store.create_run(&spec, "x").unwrap();
    step(done, RunStatus::Queued, RunStatus::Provisioning);
    step(done, RunStatus::Provisioning, Running);
    step(done, Running, Completed);
    // A run cut off while provisioning fails on recovery.
    let provisioning = store.create_run(&spec, "x").unwrap();
    step(provisioning, RunStatus::Queued, RunStatus::Provisioning);
    let running = store.create_run(&spec, "x").unwrap();
    step(running, RunStatus::Queued, RunStatus::Provisioning);
    step(running, RunStatus::Provisioning, Running);
    let unknown = RunId::new();
    // A fresh engine, whose indexer keeps copies from before the restart.
    let ix = Arc::new(FakeIndexer::default());
    lock(&ix.disk).extend([done, provisioning, running, unknown]);
    s.restart(ix);
    s.h.engine.recover().await.unwrap();
    let forgotten: HashSet<RunId> = s.ix.forgotten().into_iter().collect();
    assert_eq!(forgotten, HashSet::from([done, provisioning, unknown]));
    assert_eq!(*lock(&s.ix.disk), HashSet::from([running]));
}

#[tokio::test(start_paused = true)]
async fn recovery_retries_a_forget_that_fails_while_the_engine_runs() {
    let mut s = setup(4, LIMITS);
    let store = s.h.store.clone();
    let done = store
        .create_run(&clone_spec(&s.repo, &["travsr.get_callers"]), "x")
        .unwrap();
    store
        .transition(done, &RunStatus::Queued, RunStatus::Cancelled)
        .unwrap();
    let ix = Arc::new(FakeIndexer::default());
    lock(&ix.disk).insert(done);
    ix.forget_failures.store(1, Ordering::SeqCst);
    s.restart(ix);
    s.h.engine.recover().await.unwrap();
    let logs = travsr_logs(&s.h, done);
    assert!(logs[0].contains("could not forget"), "{logs:?}");
    s.h.start();
    until("retried", || s.ix.forgotten().len() == 2).await;
    assert!(lock(&s.ix.disk).is_empty());
}

#[tokio::test(start_paused = true)]
async fn shutdown_releases_the_runs_it_stops_and_keeps_their_copies() {
    let mut s = setup(4, LIMITS);
    let _session = s.h.driver.live();
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("running", || s.h.is(id, &Running)).await;
    let report = s.h.engine.shutdown(Duration::from_secs(5)).await;
    assert_eq!(report.destroyed, [id]);
    assert_eq!(s.ix.released(), [id]);
    assert!(s.ix.forgotten().is_empty());
    assert!(!s.h.engine.travsr_ready(id));
}

#[tokio::test(start_paused = true)]
async fn indexing_done_before_the_sandbox_is_made_still_starts_the_agent_with_travsr() {
    let mut s = setup(4, LIMITS);
    let create = s.h.driver.gate_create();
    let _session = s.h.driver.live();
    s.h.start();
    let id =
        s.h.submit(clone_spec(
            &s.repo,
            &["travsr.get_callers", "travsr.get_callers", "kestrel.join"],
        ))
        .await;
    until("indexed", || s.ix.finished.load(Ordering::SeqCst) == 1).await;
    assert_eq!(s.h.driver.starts(), 0);
    create.notify_one();
    until("running", || s.h.is(id, &Running)).await;
    // Listed once, however often the capability names it.
    assert_eq!(
        s.h.engine.travsr_tools(id).unwrap(),
        Some(vec!["get_callers".to_string()])
    );
}

#[tokio::test(start_paused = true)]
async fn the_wall_clock_still_runs_while_the_agent_waits_for_its_index() {
    let mut s = setup(4, LIMITS);
    let _gate = s.ix.close();
    s.h.start();
    let mut spec = clone_spec(&s.repo, &["travsr.get_callers"]);
    spec.budget.wall_clock = Duration::from_secs(2);
    let id = s.h.submit(spec).await;
    until("failed", || s.h.status(id).is_terminal()).await;
    let failed = s.h.is_failed(id).expect("the run failed");
    assert!(failed.contains("wall"), "{failed}");
    assert_eq!(s.h.driver.starts(), 0);
    until("no sandbox left", || s.h.driver.leaked().is_empty()).await;
}

#[test]
fn the_default_limits_are_five_minutes_two_jobs_a_retry_a_minute_and_a_minute_to_forget() {
    let limits = IndexLimits::default();
    assert_eq!(limits.timeout, Duration::from_secs(300));
    assert_eq!(limits.max_jobs, 2);
    assert_eq!(limits.retry_every, Duration::from_secs(60));
    assert_eq!(limits.forget_timeout, Duration::from_secs(60));
}
