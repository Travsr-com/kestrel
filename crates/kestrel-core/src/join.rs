//! Fan-in: [`Engine::join`] waits for a parent's children and returns their results.
//!
//! A parent that waits gives its concurrency slot back, so its children can use it, and takes it
//! back through the same first-come-first-served line as queued runs when the join ends. Without
//! this, a parent that fans out to N children with a cap of N would deadlock: it would hold a slot
//! its children need.
//!
//! A join reads everything from the store, so it can be called again at any time, after a restart
//! too: children that already ended return at once.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use kestrel_store::StoreError;
use kestrel_types::{EventKind, MicroUsd, RunId, RunStatus};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::{Engine, Inner, Rejoin};

/// When a join returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinMode {
    /// Once every child has ended.
    All,
    /// Once any child has ended. Only the children that ended are returned.
    Any,
}

/// How a child ended, as a join reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinOutcome {
    /// With the summary the agent gave when it finished, if any.
    Completed {
        summary: Option<String>,
    },
    Failed {
        reason: String,
    },
    Cancelled,
    /// Still going when the join's timeout came. It is not cancelled.
    TimedOut,
}

/// One child's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildResult {
    pub run: RunId,
    pub outcome: JoinOutcome,
    /// What the child spent itself.
    pub spent: MicroUsd,
}

/// Why [`Engine::join`] did not wait.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("no run {0}")]
    UnknownRun(RunId),
    /// Only a run's own children can be joined.
    #[error("run {0} is not a child of the caller")]
    NotYourChild(RunId),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Engine {
    /// Waits until `children` of `parent` end (every one for [`JoinMode::All`], the first for
    /// [`JoinMode::Any`]), or until `timeout`, and returns each child's result. An empty
    /// `children` means every child of `parent`.
    ///
    /// While it waits, `parent` holds no concurrency slot: it gives its slot back when it starts
    /// waiting and takes it back, in line with queued runs, before this returns. Its sandbox keeps
    /// running meanwhile (it is in a tool call), so running sandboxes can briefly outnumber the
    /// slots by the parents that wait. A join whose children have all ended already returns at
    /// once and keeps the slot. A child that times out keeps running.
    ///
    /// Overlapping joins of one parent share this: the first gives the slot back, the last takes it
    /// again. With a timeout, the wait for the slot is bounded too: a join past its deadline
    /// returns without the slot, the parent runs over the cap for a moment, and takes the slot back
    /// once one is free. A join dropped while it waits (its caller went away) also leaves the slot
    /// to come back to the parent later.
    ///
    /// Taking the slot back needs [`Engine::run_forever`]; a join without a timeout that has to
    /// wait therefore only returns while the engine runs.
    pub async fn join(
        &self,
        parent: RunId,
        children: &[RunId],
        mode: JoinMode,
        timeout: Option<Duration>,
    ) -> Result<Vec<ChildResult>, JoinError> {
        let store = &self.inner.store;
        store
            .get_run(parent)?
            .ok_or(JoinError::UnknownRun(parent))?;
        let mut ids: Vec<RunId> = Vec::new();
        if children.is_empty() {
            ids = store.children(parent)?.into_iter().map(|r| r.id).collect();
        }
        for id in children {
            let row = store.get_run(*id)?.ok_or(JoinError::UnknownRun(*id))?;
            if row.parent != Some(parent) {
                return Err(JoinError::NotYourChild(*id));
            }
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        let mut ended = HashMap::new();
        if let Some(done) = self.ready(&ids, mode, &mut ended)? {
            return Ok(done);
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        self.inner.enter_join(parent);
        let mut waiting = Waiting {
            inner: self.inner.clone(),
            parent,
            left: false,
        };
        let result = self.wait(&ids, mode, deadline, &mut ended).await;
        waiting.left = true;
        if self.inner.leave_join(parent) {
            self.take_slot_back(parent, deadline).await;
        }
        result
    }

    async fn wait(
        &self,
        ids: &[RunId],
        mode: JoinMode,
        deadline: Option<Instant>,
        ended: &mut HashMap<RunId, ChildResult>,
    ) -> Result<Vec<ChildResult>, JoinError> {
        let period = self
            .inner
            .limits
            .poll_interval
            .max(Duration::from_millis(1));
        loop {
            if let Some(done) = self.ready(ids, mode, ended)? {
                return Ok(done);
            }
            let now = Instant::now();
            if deadline.is_some_and(|d| now >= d) {
                let mut results = Vec::new();
                for id in ids {
                    results.push(match ended.get(id) {
                        Some(result) => result.clone(),
                        None => ChildResult {
                            run: *id,
                            outcome: JoinOutcome::TimedOut,
                            spent: self.inner.store.spent(*id)?,
                        },
                    });
                }
                return Ok(results);
            }
            let next = match deadline {
                Some(d) => (now + period).min(d),
                None => now + period,
            };
            tokio::time::sleep_until(next).await;
        }
    }

    /// The results when `mode` is satisfied, else `None`. Children that ended are kept in
    /// `ended`, so each one's events are read once.
    fn ready(
        &self,
        ids: &[RunId],
        mode: JoinMode,
        ended: &mut HashMap<RunId, ChildResult>,
    ) -> Result<Option<Vec<ChildResult>>, JoinError> {
        for id in ids {
            if !ended.contains_key(id)
                && let Some(result) = self.result(*id)?
            {
                ended.insert(*id, result);
            }
        }
        let done: Vec<ChildResult> = ids.iter().filter_map(|id| ended.get(id).cloned()).collect();
        let satisfied = match mode {
            JoinMode::All => done.len() == ids.len(),
            JoinMode::Any => !done.is_empty() || ids.is_empty(),
        };
        Ok(satisfied.then_some(done))
    }

    /// The child's result once it has ended, else `None`.
    fn result(&self, run: RunId) -> Result<Option<ChildResult>, JoinError> {
        let store = &self.inner.store;
        let Some(row) = store.get_run(run)? else {
            return Err(JoinError::UnknownRun(run));
        };
        let outcome = match row.status {
            RunStatus::Completed => {
                let summary =
                    store
                        .events_since(run, 0)?
                        .into_iter()
                        .rev()
                        .find_map(|e| match e.kind {
                            EventKind::RunFinished { summary } => Some(summary),
                            _ => None,
                        });
                JoinOutcome::Completed {
                    summary: summary.flatten(),
                }
            }
            RunStatus::Failed(reason) => JoinOutcome::Failed { reason },
            RunStatus::Cancelled => JoinOutcome::Cancelled,
            _ => return Ok(None),
        };
        Ok(Some(ChildResult {
            run,
            outcome,
            spent: store.spent(run)?,
        }))
    }

    /// Lines up for the slot like a queued run and waits until the scheduler grants it, at most
    /// until `deadline`. When it does not come by then, or this future is dropped, a detached
    /// request stays in line and the slot comes back to the parent later.
    async fn take_slot_back(&self, run: RunId, deadline: Option<Instant>) {
        let mut back = SlotBack {
            inner: self.inner.clone(),
            run,
            done: false,
        };
        let (tx, rx) = oneshot::channel();
        self.inner.rejoin(run, Rejoin::Told(tx));
        back.done = match deadline {
            Some(d) => matches!(tokio::time::timeout_at(d, rx).await, Ok(Ok(()))),
            None => rx.await.is_ok(),
        };
    }
}

/// A join that waits. Dropped before it ends (the caller went away), it still gives the parent
/// its slot back when it was the last join waiting.
struct Waiting {
    inner: Arc<Inner>,
    parent: RunId,
    left: bool,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if !self.left && self.inner.leave_join(self.parent) {
            self.inner.rejoin(self.parent, Rejoin::Detached);
        }
    }
}

/// A join waiting for its slot back. Dropped or timed out before it came, it leaves a detached
/// request, so the parent still gets the slot once one is free.
struct SlotBack {
    inner: Arc<Inner>,
    run: RunId,
    done: bool,
}

impl Drop for SlotBack {
    fn drop(&mut self) {
        if !self.done {
            self.inner.rejoin(self.run, Rejoin::Detached);
        }
    }
}
