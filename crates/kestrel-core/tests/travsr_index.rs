//! Issue #44: the engine has each run's clone indexed for Travsr before its agent starts, and
//! forgotten when its sandbox goes, through a fake `RunIndexer`.
//!
//! Every test runs on a paused clock with the fake driver and adapter from `common`, and a fake
//! git that makes a clone directory. Test names start with the scenario number of the issue they
//! cover (`s1_...`); rules the engine adds on top have no number.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use kestrel_core::caps::RootKey;
use kestrel_core::{IndexLimits, RunIndexer, Workspaces};
use kestrel_sandbox::{CommandOutput, CommandRunner, SpawnedProcess};
use kestrel_store::ControlCommand;
use kestrel_types::{AgentSpec, EventKind, RunId, RunStatus, WorkspaceSpec};
use tempfile::TempDir;
use tokio::sync::Semaphore;

use RunStatus::{Completed, Running, Suspended};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Git that succeeds; `clone` makes the target directory with a `.git` entry.
struct FakeGit;

#[async_trait]
impl CommandRunner for FakeGit {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        if argv.get(1).is_some_and(|a| a == "clone") {
            let path = PathBuf::from(argv.last().expect("clone target"));
            std::fs::create_dir_all(path.join(".git"))?;
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

/// A fake Travsr indexer: records every call, and can wait, fail or take time.
#[derive(Default)]
struct FakeIndexer {
    indexed: Mutex<Vec<(RunId, PathBuf)>>,
    forgotten: Mutex<Vec<(RunId, PathBuf)>>,
    /// When set, `index` waits for a permit of it (one per call).
    gate: Mutex<Option<Arc<Semaphore>>>,
    /// How long `index` takes.
    takes: Mutex<Duration>,
    /// When set, `index` fails with this.
    fails: Mutex<Option<String>>,
    /// How many of the next `forget` calls fail.
    forget_failures: AtomicUsize,
    running: AtomicUsize,
    most_running: AtomicUsize,
    finished: AtomicUsize,
}

impl FakeIndexer {
    fn indexed(&self) -> Vec<(RunId, PathBuf)> {
        lock(&self.indexed).clone()
    }

    fn forgotten(&self) -> Vec<(RunId, PathBuf)> {
        lock(&self.forgotten).clone()
    }

    /// Makes `index` wait until [`FakeIndexer::open`].
    fn close(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *lock(&self.gate) = Some(gate.clone());
        gate
    }
}

#[async_trait]
impl RunIndexer for FakeIndexer {
    async fn index(&self, run: RunId, clone: &Path) -> Result<(), String> {
        lock(&self.indexed).push((run, clone.to_path_buf()));
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

    async fn forget(&self, run: RunId, clone: &Path) -> Result<(), String> {
        lock(&self.forgotten).push((run, clone.to_path_buf()));
        let failing = self
            .forget_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if failing {
            Err("travsr repos --remove exited with code 1".into())
        } else {
            Ok(())
        }
    }
}

const LIMITS: IndexLimits = IndexLimits {
    timeout: Duration::from_secs(5),
    max_jobs: 2,
    retry_every: Duration::from_secs(10),
};

struct Setup {
    h: Harness,
    ix: Arc<FakeIndexer>,
    /// The source repo every clone is made from, and the workspaces root.
    repo: PathBuf,
    root: PathBuf,
    _dir: TempDir,
}

/// A harness whose engine has workspaces (fake git), the fake indexer with `limits` and a root key.
fn setup(max: usize, limits: IndexLimits) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let root = dir.path().join("workspaces");
    std::fs::create_dir_all(&repo).unwrap();
    let mut h = Harness::new(max);
    let ix = Arc::new(FakeIndexer::default());
    let (ws_root, indexer) = (root.clone(), ix.clone());
    h.rebuild(move |e| {
        e.with_workspaces(Workspaces::new(ws_root, Arc::new(FakeGit)))
            .with_root_key(RootKey::generate())
            .with_indexer(indexer, limits)
    });
    Setup {
        h,
        ix,
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

#[tokio::test(start_paused = true)]
async fn s1_the_clone_is_indexed_before_the_agent_starts() {
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
    let clone = s.root.join(id.to_string());
    until("indexing started", || s.ix.indexed().len() == 1).await;
    assert_eq!(s.ix.indexed(), [(id, clone.clone())]);
    // The sandbox may be made meanwhile, but the agent waits for the index.
    settle().await;
    assert_eq!(s.h.driver.starts(), 0);
    assert!(!s.h.is(id, &Running));
    assert_eq!(s.h.engine.travsr_tools(id).unwrap(), Some(Vec::new()));
    assert_eq!(s.h.engine.travsr_clone(id), None);

    gate.add_permits(1);
    until("running", || s.h.is(id, &Running)).await;
    assert_eq!(s.h.driver.starts(), 1);
    assert_eq!(
        s.h.engine.travsr_tools(id).unwrap(),
        Some(vec!["get_callers".to_string(), "search_symbol".to_string()])
    );
    assert_eq!(s.h.engine.travsr_clone(id), Some(clone.clone()));
    assert!(
        travsr_logs(&s.h, id).is_empty(),
        "{:?}",
        travsr_logs(&s.h, id)
    );

    // Scenario 8: the run finishes, its clone is forgotten.
    session.line("ok:done");
    session.exit(0);
    until("completed", || s.h.is(id, &Completed)).await;
    until("forgotten", || !s.ix.forgotten().is_empty()).await;
    assert_eq!(s.ix.forgotten(), [(id, clone)]);
    assert_eq!(s.h.engine.travsr_tools(id).unwrap(), None);
    assert_eq!(s.h.engine.travsr_clone(id), None);
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
    assert_eq!(s.h.engine.travsr_clone(id), None);
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
    let clone = s.root.join(id.to_string());
    assert_eq!(s.ix.forgotten(), [(id, clone.clone()), (id, clone)]);
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
    let indexed: HashMap<RunId, PathBuf> = s.ix.indexed().into_iter().collect();
    for child in &children {
        // Each child's own clone, not its parent's.
        assert_eq!(indexed[child], s.root.join(child.to_string()));
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
}

#[tokio::test(start_paused = true)]
async fn without_an_indexer_nothing_is_indexed_and_no_travsr_tool_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut h = Harness::new(4);
    let root = dir.path().join("workspaces");
    h.rebuild(move |e| e.with_workspaces(Workspaces::new(root, Arc::new(FakeGit))));
    let _session = h.driver.live();
    h.start();
    let id = h.submit(clone_spec(&repo, &["travsr.get_callers"])).await;
    until("running", || h.is(id, &Running)).await;
    assert_eq!(h.engine.travsr_tools(id).unwrap(), Some(Vec::new()));
    assert_eq!(h.engine.travsr_clone(id), None);
    assert_eq!(h.engine.travsr_tools(RunId::new()).unwrap(), None);
}

#[tokio::test(start_paused = true)]
async fn a_run_cancelled_while_indexing_never_starts_and_its_clone_is_forgotten() {
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
    assert_eq!(s.ix.forgotten()[0].1, s.root.join(id.to_string()));
    assert!(s.h.driver.leaked().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_run_resumed_after_a_restart_is_forgotten_by_recovery_and_indexed_again() {
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

    // A new engine on the same database, workspaces and Travsr.
    let ix = Arc::new(FakeIndexer::default());
    let (root, indexer) = (s.root.clone(), ix.clone());
    s.h.shutdown = tokio_util::sync::CancellationToken::new();
    s.h.rebuild(move |e| {
        e.with_workspaces(Workspaces::new(root, Arc::new(FakeGit)))
            .with_root_key(RootKey::generate())
            .with_indexer(indexer, LIMITS)
    });
    s.h.engine.recover().await.unwrap();
    let clone = s.root.join(id.to_string());
    assert_eq!(ix.forgotten(), [(id, clone.clone())]);
    assert!(matches!(s.h.status(id), Suspended(_)));
    let _second = s.h.driver.live();
    s.h.start();
    s.h.control(id, ControlCommand::Resume);
    until("running again", || s.h.is(id, &Running)).await;
    assert_eq!(ix.indexed(), [(id, clone.clone())]);
    assert_eq!(s.h.engine.travsr_clone(id), Some(clone));
}

#[tokio::test(start_paused = true)]
async fn shutdown_forgets_the_clones_of_the_sandboxes_it_destroys() {
    let mut s = setup(4, LIMITS);
    let _session = s.h.driver.live();
    s.h.start();
    let id =
        s.h.submit(clone_spec(&s.repo, &["travsr.get_callers"]))
            .await;
    until("running", || s.h.is(id, &Running)).await;
    let report = s.h.engine.shutdown(Duration::from_secs(5)).await;
    assert_eq!(report.destroyed, [id]);
    assert_eq!(s.ix.forgotten(), [(id, s.root.join(id.to_string()))]);
    assert_eq!(s.h.engine.travsr_clone(id), None);
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
fn the_default_limits_are_five_minutes_two_jobs_and_a_retry_a_minute() {
    let limits = IndexLimits::default();
    assert_eq!(limits.timeout, Duration::from_secs(300));
    assert_eq!(limits.max_jobs, 2);
    assert_eq!(limits.retry_every, Duration::from_secs(60));
}
