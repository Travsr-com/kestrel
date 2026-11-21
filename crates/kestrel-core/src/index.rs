//! Indexing each run's clone for Travsr, see [`Engine::with_indexer`](crate::Engine::with_indexer).
//!
//! The engine hands a run's clone to a [`RunIndexer`] after the clone exists and before its agent
//! starts, and has it forgotten when the run's sandbox goes. What it remembers:
//!
//! * which clones it handed over and not forgotten yet, and which of them are ready, so the
//!   gateway lists Travsr tools only for a run whose clone is indexed;
//! * which forgets failed, so they are tried again later. That list lives in memory: a forget
//!   that still fails when the engine stops is not retried by the next one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use kestrel_types::{AgentSpec, RunId};
use tokio::sync::Semaphore;

/// Indexes a run's clone for Travsr and forgets it again. The `Err` text says why, for the run's
/// log.
#[async_trait]
pub trait RunIndexer: Send + Sync {
    /// Indexes `clone`, the run's workspace; returns once the index is ready. The engine drops
    /// the future when it gives up waiting, so dropping it should stop the work.
    async fn index(&self, run: RunId, clone: &Path) -> Result<(), String>;
    /// Forgets `clone`: Travsr no longer knows it, and nothing is kept running for it.
    async fn forget(&self, run: RunId, clone: &Path) -> Result<(), String>;
}

/// How the engine uses a [`RunIndexer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLimits {
    /// The longest one indexing job may take. Past it, the run starts without Travsr. A run may
    /// first wait for a job slot, see `max_jobs`.
    pub timeout: Duration,
    /// Most indexing jobs at once. Runs past it wait for a slot, in turn.
    pub max_jobs: usize,
    /// How often failed forgets are tried again.
    pub retry_every: Duration,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(300),
            max_jobs: 2,
            retry_every: Duration::from_secs(60),
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
    /// Clones handed to the indexer and not forgotten yet, by run, and whether each is ready.
    clones: Mutex<HashMap<RunId, (PathBuf, bool)>>,
    /// Clones whose forget failed, by run, to try again.
    failed: Mutex<HashMap<RunId, PathBuf>>,
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
            clones: Mutex::default(),
            failed: Mutex::default(),
            retrying: AtomicBool::new(false),
        }
    }

    pub(crate) fn retry_every(&self) -> Duration {
        self.limits.retry_every
    }

    /// Indexes `run`'s `clone`: waits for a job slot, then at most the timeout. `Err` says why
    /// the clone is not ready.
    pub(crate) async fn index(&self, run: RunId, clone: PathBuf) -> Result<(), String> {
        lock(&self.clones).insert(run, (clone.clone(), false));
        // The semaphore is never closed, so this always gets a slot.
        let _slot = self.jobs.acquire().await.ok();
        let limit = self.limits.timeout;
        match tokio::time::timeout(limit, self.host.index(run, &clone)).await {
            Ok(Ok(())) => {
                if let Some((_, ready)) = lock(&self.clones).get_mut(&run) {
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

    /// `run`'s clone, once it is indexed.
    pub(crate) fn ready(&self, run: RunId) -> Option<PathBuf> {
        lock(&self.clones)
            .get(&run)
            .filter(|(_, ready)| *ready)
            .map(|(clone, _)| clone.clone())
    }

    /// Forgets `run`'s clone: the one it was handed, else one whose forget failed, else
    /// `fallback`. `None` when there is none; `Some(Err)` when the forget failed, and it is kept
    /// for [`Indexing::retry`].
    pub(crate) async fn forget(
        &self,
        run: RunId,
        fallback: Option<PathBuf>,
    ) -> Option<Result<(), String>> {
        let handed = lock(&self.clones).remove(&run).map(|(clone, _)| clone);
        let clone = handed
            .or_else(|| lock(&self.failed).remove(&run))
            .or(fallback)?;
        let forgotten = self.host.forget(run, &clone).await;
        if forgotten.is_err() {
            lock(&self.failed).insert(run, clone);
        }
        Some(forgotten)
    }

    /// Tries every failed forget again, one retry at a time. Each stays listed until it works, so
    /// a retry that is dropped halfway loses none.
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
        let failed: Vec<(RunId, PathBuf)> = lock(&self.failed)
            .iter()
            .map(|(run, clone)| (*run, clone.clone()))
            .collect();
        for (run, clone) in failed {
            if self.host.forget(run, &clone).await.is_ok() {
                let mut failed = lock(&self.failed);
                if failed.get(&run) == Some(&clone) {
                    failed.remove(&run);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Forgets fail until `works` is set; each forget waits for `gate` when there is one.
    #[derive(Default)]
    struct Host {
        forgets: AtomicUsize,
        works: AtomicBool,
        gate: Mutex<Option<Arc<Semaphore>>>,
    }

    #[async_trait]
    impl RunIndexer for Host {
        async fn index(&self, _: RunId, _: &Path) -> Result<(), String> {
            Ok(())
        }
        async fn forget(&self, _: RunId, _: &Path) -> Result<(), String> {
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
    }

    #[tokio::test]
    async fn one_retry_at_a_time_and_a_dropped_retry_loses_nothing() {
        let host = Arc::new(Host::default());
        let ix = Arc::new(Indexing::new(host.clone(), IndexLimits::default()));
        let (run, clone) = (RunId::new(), PathBuf::from("/w/1"));
        assert_eq!(ix.index(run, clone.clone()).await, Ok(()));
        assert_eq!(ix.ready(run), Some(clone.clone()));
        assert_eq!(ix.forget(RunId::new(), None).await, None);
        assert_eq!(ix.forget(run, None).await, Some(Err("no".to_string())));
        assert_eq!(ix.ready(run), None);
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
}
