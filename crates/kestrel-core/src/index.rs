//! Indexing each run's code for Travsr, see [`Engine::with_indexer`](crate::Engine::with_indexer).
//!
//! Travsr runs on the host, so it never reads a run's workspace, which the agent may rewrite.
//! The engine hands the indexer an [`IndexSource`]: the source repo the workspace was cloned
//! from and the commit it was cloned at. The indexer keeps its own copy of that commit, out of
//! every sandbox, and indexes and serves that. What the engine remembers:
//!
//! * which runs it handed over and not forgotten yet, and which of them are ready, so the
//!   gateway lists Travsr tools only for a run whose copy is indexed, and only a run handed
//!   over (or listed as kept after a restart) is ever forgotten;
//! * which forgets failed, so they are tried again later. That list lives in memory; a restart
//!   finds what is left through [`RunIndexer::kept`] instead.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use kestrel_types::{AgentSpec, RunId};
use tokio::sync::Semaphore;

/// What a run's code is, for the indexer: never the run's workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSource {
    /// The `git-clone` source repo the run's workspace was cloned from.
    pub repo: PathBuf,
    /// The commit the workspace was cloned at, read before its sandbox existed. `None` when the
    /// run is resumed with the workspace it had: the agent may have changed it, so the indexer
    /// uses the commit it recorded when it first indexed the run.
    pub commit: Option<String>,
}

/// Indexes a run's code for Travsr, serves it, and forgets it again. The `Err` text says why, for
/// the run's log.
#[async_trait]
pub trait RunIndexer: Send + Sync {
    /// Indexes `source` for `run`, in a copy of the indexer's own; returns once the index is
    /// ready. A run indexed before reuses its copy. The engine drops the future when it gives up
    /// waiting, so dropping it should stop the work.
    async fn index(&self, run: RunId, source: &IndexSource) -> Result<(), String>;
    /// Stops serving `run`, keeping its copy, for a run that may be resumed.
    async fn release(&self, run: RunId);
    /// Forgets `run` for good: nothing is kept running or on disk for it.
    async fn forget(&self, run: RunId) -> Result<(), String>;
    /// The runs the indexer keeps a copy for, so recovery can forget those that ended.
    async fn kept(&self) -> Result<Vec<RunId>, String>;
}

/// How the engine uses a [`RunIndexer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLimits {
    /// The longest a run waits for its index, the wait for a job slot (see `max_jobs`)
    /// included. Past it, the run starts without Travsr.
    pub timeout: Duration,
    /// Most indexing jobs at once. Runs past it wait for a slot, in turn.
    pub max_jobs: usize,
    /// How often failed forgets are tried again.
    pub retry_every: Duration,
    /// The longest one forget, or one release, may take. Past it, a forget counts as failed.
    pub forget_timeout: Duration,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(300),
            max_jobs: 2,
            retry_every: Duration::from_secs(60),
            forget_timeout: Duration::from_secs(60),
        }
    }
}

/// Whether `spec` grants any Travsr tool (`travsr.<tool>`). Only such runs are indexed.
pub(crate) fn wants_travsr(spec: &AgentSpec) -> bool {
    spec.capabilities
        .mcp
        .iter()
        .any(|tool| tool.starts_with("travsr."))
}

/// The engine's side of indexing: the indexer, its job slots and what it was handed.
pub(crate) struct Indexing {
    host: Arc<dyn RunIndexer>,
    limits: IndexLimits,
    jobs: Semaphore,
    /// Runs being indexed or served (not released or forgotten yet), and whether each is ready.
    runs: Mutex<HashMap<RunId, bool>>,
    /// Runs the indexer may keep something for: handed to it (it got a job slot) or listed by
    /// [`RunIndexer::kept`], and not forgotten since. Only these are forgotten, so a run that
    /// never reached the indexer never asks it for anything.
    handed: Mutex<HashSet<RunId>>,
    /// Runs whose forget failed, to try again.
    failed: Mutex<HashSet<RunId>>,
    /// A retry is under way.
    retrying: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Indexing {
    pub(crate) fn new(host: Arc<dyn RunIndexer>, limits: IndexLimits) -> Indexing {
        Indexing {
            host,
            limits,
            jobs: Semaphore::new(limits.max_jobs.max(1)),
            runs: Mutex::default(),
            handed: Mutex::default(),
            failed: Mutex::default(),
            retrying: AtomicBool::new(false),
        }
    }

    pub(crate) fn retry_every(&self) -> Duration {
        self.limits.retry_every
    }

    /// Indexes `run`'s `source`: waits for a job slot, then for the index, both together at
    /// most the timeout. `Err` says why the run is not ready. A forget still to retry for the
    /// run is dropped: the run is in use.
    pub(crate) async fn index(&self, run: RunId, source: IndexSource) -> Result<(), String> {
        lock(&self.failed).remove(&run);
        lock(&self.runs).insert(run, false);
        let limit = self.limits.timeout;
        let deadline = tokio::time::Instant::now() + limit;
        // The semaphore is never closed, so a slot that comes free in time is always had.
        let Ok(slot) = tokio::time::timeout_at(deadline, self.jobs.acquire()).await else {
            return Err(format!(
                "no indexing slot came free within {} s",
                limit.as_secs()
            ));
        };
        let _slot = slot.ok();
        lock(&self.handed).insert(run);
        match tokio::time::timeout_at(deadline, self.host.index(run, &source)).await {
            Ok(Ok(())) => {
                if let Some(ready) = lock(&self.runs).get_mut(&run) {
                    *ready = true;
                }
                Ok(())
            }
            Ok(Err(e)) => Err(format!("indexing failed: {e}")),
            Err(_) => Err(format!(
                "indexing did not finish within {} s",
                limit.as_secs()
            )),
        }
    }

    /// Whether `run`'s copy is indexed and served.
    pub(crate) fn ready(&self, run: RunId) -> bool {
        lock(&self.runs).get(&run).copied().unwrap_or(false)
    }

    /// Stops serving `run` and keeps its copy, at most the forget timeout.
    pub(crate) async fn release(&self, run: RunId) {
        if lock(&self.runs).remove(&run).is_some() {
            let _ = tokio::time::timeout(self.limits.forget_timeout, self.host.release(run)).await;
        }
    }

    /// Forgets `run`, at most the forget timeout. A failure is kept for [`Indexing::retry`]. A
    /// run the indexer was never handed, and does not list as kept, has nothing to forget: the
    /// indexer is not asked, so a run without Travsr never starts a Travsr process.
    pub(crate) async fn forget(&self, run: RunId) -> Result<(), String> {
        lock(&self.runs).remove(&run);
        lock(&self.failed).remove(&run);
        if !lock(&self.handed).contains(&run) {
            return Ok(());
        }
        let forgotten = self.bounded_forget(run).await;
        if forgotten.is_err() {
            lock(&self.failed).insert(run);
        }
        forgotten
    }

    /// Asks the indexer to forget `run`; once it has, the run is no longer handed over.
    async fn bounded_forget(&self, run: RunId) -> Result<(), String> {
        let limit = self.limits.forget_timeout;
        let forgotten = match tokio::time::timeout(limit, self.host.forget(run)).await {
            Ok(done) => done,
            Err(_) => Err(format!("it did not finish within {} s", limit.as_secs())),
        };
        if forgotten.is_ok() {
            lock(&self.handed).remove(&run);
        }
        forgotten
    }

    /// The runs the indexer keeps a copy for. Each one listed may be forgotten later.
    pub(crate) async fn kept(&self) -> Result<Vec<RunId>, String> {
        let limit = self.limits.forget_timeout;
        match tokio::time::timeout(limit, self.host.kept()).await {
            Ok(Ok(kept)) => {
                lock(&self.handed).extend(kept.iter().copied());
                Ok(kept)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!(
                "listing did not finish within {} s",
                limit.as_secs()
            )),
        }
    }

    /// Tries every failed forget again, one retry at a time. Each stays listed until it works, so
    /// a retry that is dropped halfway loses none. A run indexed again since is in use, and is
    /// not forgotten.
    pub(crate) async fn retry(&self) {
        /// Clears the flag however the retry ends.
        struct Retrying<'a>(&'a AtomicBool);
        impl Drop for Retrying<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        if self.retrying.swap(true, Ordering::SeqCst) {
            return;
        }
        let _retrying = Retrying(&self.retrying);
        let failed: Vec<RunId> = lock(&self.failed).iter().copied().collect();
        for run in failed {
            if lock(&self.runs).contains_key(&run) || !lock(&self.failed).contains(&run) {
                lock(&self.failed).remove(&run);
                continue;
            }
            if self.bounded_forget(run).await.is_ok() {
                lock(&self.failed).remove(&run);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Forgets fail until `works` is set; each forget waits for `gate` when there is one. An index
    /// takes `takes`, and `kept` lists `kept`.
    #[derive(Default)]
    struct Host {
        indexes: AtomicUsize,
        takes: Mutex<Duration>,
        forgets: AtomicUsize,
        works: AtomicBool,
        gate: Mutex<Option<Arc<Semaphore>>>,
        kept: Mutex<Vec<RunId>>,
    }

    #[async_trait]
    impl RunIndexer for Host {
        async fn index(&self, _: RunId, _: &IndexSource) -> Result<(), String> {
            self.indexes.fetch_add(1, Ordering::SeqCst);
            let takes = *lock(&self.takes);
            tokio::time::sleep(takes).await;
            Ok(())
        }
        async fn release(&self, _: RunId) {}
        async fn forget(&self, _: RunId) -> Result<(), String> {
            self.forgets.fetch_add(1, Ordering::SeqCst);
            let gate = lock(&self.gate).clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            if self.works.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("no".into())
            }
        }
        async fn kept(&self) -> Result<Vec<RunId>, String> {
            Ok(lock(&self.kept).clone())
        }
    }

    fn source() -> IndexSource {
        IndexSource {
            repo: PathBuf::from("/srv/repo"),
            commit: None,
        }
    }

    #[tokio::test]
    async fn one_retry_at_a_time_and_a_dropped_retry_loses_nothing() {
        let host = Arc::new(Host::default());
        let ix = Arc::new(Indexing::new(host.clone(), IndexLimits::default()));
        let run = RunId::new();
        assert_eq!(ix.index(run, source()).await, Ok(()));
        assert!(ix.ready(run));
        assert_eq!(ix.forget(run).await, Err("no".to_string()));
        assert!(!ix.ready(run));
        // A retry that waits, a second one that finds it under way, then the first is dropped.
        let gate = Arc::new(Semaphore::new(0));
        *lock(&host.gate) = Some(gate.clone());
        let first = tokio::spawn({
            let ix = ix.clone();
            async move { ix.retry().await }
        });
        while host.forgets.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        ix.retry().await;
        assert_eq!(host.forgets.load(Ordering::SeqCst), 2);
        first.abort();
        let _ = first.await;
        // Still listed, and retried once the host works.
        *lock(&host.gate) = None;
        host.works.store(true, Ordering::SeqCst);
        ix.retry().await;
        assert_eq!(host.forgets.load(Ordering::SeqCst), 3);
        assert!(lock(&ix.failed).is_empty());
        ix.retry().await;
        assert_eq!(host.forgets.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_failed_forget_never_hits_a_run_indexed_again() {
        let host = Arc::new(Host::default());
        let ix = Indexing::new(host.clone(), IndexLimits::default());
        let (resumed, listed) = (RunId::new(), RunId::new());
        for run in [resumed, listed] {
            ix.index(run, source()).await.unwrap();
        }
        ix.forget(resumed).await.unwrap_err();
        ix.forget(listed).await.unwrap_err();
        assert_eq!(host.forgets.load(Ordering::SeqCst), 2);
        // Indexing clears its forget.
        ix.index(resumed, source()).await.unwrap();
        assert!(!lock(&ix.failed).contains(&resumed));
        // And a retry already holding the list skips a run handed over again.
        lock(&ix.failed).insert(resumed);
        host.works.store(true, Ordering::SeqCst);
        ix.retry().await;
        assert_eq!(host.forgets.load(Ordering::SeqCst), 3, "only `listed`");
        assert!(lock(&ix.failed).is_empty());
        assert!(ix.ready(resumed));
    }

    #[tokio::test]
    async fn only_a_run_handed_to_the_indexer_or_kept_by_it_is_forgotten() {
        let host = Arc::new(Host::default());
        host.works.store(true, Ordering::SeqCst);
        let ix = Indexing::new(host.clone(), IndexLimits::default());
        // Never handed over: nothing to forget, and the indexer is not asked.
        let never = RunId::new();
        assert_eq!(ix.forget(never).await, Ok(()));
        assert_eq!(host.forgets.load(Ordering::SeqCst), 0);
        // Kept from before a restart: forgotten once.
        let kept = RunId::new();
        lock(&host.kept).push(kept);
        assert_eq!(ix.kept().await, Ok(vec![kept]));
        assert_eq!(ix.forget(kept).await, Ok(()));
        assert_eq!(host.forgets.load(Ordering::SeqCst), 1);
        assert_eq!(ix.forget(kept).await, Ok(()));
        assert_eq!(host.forgets.load(Ordering::SeqCst), 1);
        // Handed over and released (served no more, its copy kept): still forgotten later.
        let released = RunId::new();
        ix.index(released, source()).await.unwrap();
        ix.release(released).await;
        assert_eq!(ix.forget(released).await, Ok(()));
        assert_eq!(host.forgets.load(Ordering::SeqCst), 2);
        // A failed forget stays listed until a retry works, then is not tried again.
        host.works.store(false, Ordering::SeqCst);
        let failing = RunId::new();
        ix.index(failing, source()).await.unwrap();
        ix.forget(failing).await.unwrap_err();
        host.works.store(true, Ordering::SeqCst);
        ix.retry().await;
        assert_eq!(host.forgets.load(Ordering::SeqCst), 4);
        assert_eq!(ix.forget(failing).await, Ok(()));
        assert_eq!(host.forgets.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn the_timeout_covers_the_wait_for_a_slot() {
        let limits = IndexLimits {
            timeout: Duration::from_secs(5),
            max_jobs: 1,
            ..IndexLimits::default()
        };
        let host = Arc::new(Host::default());
        let ix = Indexing::new(host.clone(), limits);
        // Every slot is taken past the timeout: the run gives up without reaching the indexer.
        let held = ix.jobs.acquire().await.unwrap();
        let run = RunId::new();
        let started = tokio::time::Instant::now();
        let err = ix.index(run, source()).await.unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        assert!(
            err.contains("no indexing slot came free within 5 s"),
            "{err}"
        );
        assert_eq!(host.indexes.load(Ordering::SeqCst), 0);
        assert!(!ix.ready(run));
        // Nothing was handed over, so nothing is forgotten.
        assert_eq!(ix.forget(run).await, Ok(()));
        assert_eq!(host.forgets.load(Ordering::SeqCst), 0);
        // A slot that comes free after 2 s leaves 3 s for an index that takes 4.
        *lock(&host.takes) = Duration::from_secs(4);
        let started = tokio::time::Instant::now();
        let free_later = async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            drop(held);
        };
        let ((), indexed) = tokio::join!(free_later, ix.index(RunId::new(), source()));
        let err = indexed.unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        assert!(err.contains("did not finish within 5 s"), "{err}");
        assert_eq!(host.indexes.load(Ordering::SeqCst), 1);
        // One that takes less is ready.
        *lock(&host.takes) = Duration::from_secs(1);
        let run = RunId::new();
        ix.index(run, source()).await.unwrap();
        assert!(ix.ready(run));
    }

    #[tokio::test(start_paused = true)]
    async fn a_forget_or_a_release_that_hangs_is_given_up_after_the_timeout() {
        /// Never finishes anything.
        struct Stuck;
        #[async_trait]
        impl RunIndexer for Stuck {
            async fn index(&self, _: RunId, _: &IndexSource) -> Result<(), String> {
                Ok(())
            }
            async fn release(&self, _: RunId) {
                std::future::pending::<()>().await;
            }
            async fn forget(&self, _: RunId) -> Result<(), String> {
                std::future::pending().await
            }
            async fn kept(&self) -> Result<Vec<RunId>, String> {
                std::future::pending().await
            }
        }
        let limits = IndexLimits {
            forget_timeout: Duration::from_secs(7),
            ..IndexLimits::default()
        };
        let ix = Indexing::new(Arc::new(Stuck), limits);
        let run = RunId::new();
        ix.index(run, source()).await.unwrap();
        let err = ix.forget(run).await.unwrap_err();
        assert!(err.contains("within 7 s"), "{err}");
        assert!(lock(&ix.failed).contains(&run));
        ix.retry().await;
        assert!(lock(&ix.failed).contains(&run));
        ix.index(run, source()).await.unwrap();
        ix.release(run).await;
        assert!(!ix.ready(run));
        // A run never handed over has nothing to release.
        ix.release(RunId::new()).await;
        let err = ix.kept().await.unwrap_err();
        assert!(err.contains("within 7 s"), "{err}");
    }
}
