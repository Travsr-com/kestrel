//! Append-only event log and run state on SQLite.
//!
//! In M1 there is no HTTP server: the CLI and the engine share one SQLite file (WAL mode) and talk
//! through it. This file is the public contract from issue #2; bodies are filled in by the
//! implementation.

use std::path::Path;

use kestrel_types::{
    AgentSpec, ApprovalId, Event, EventKind, MicroUsd, ModelCallRecord, RunId, RunStatus,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Schema version this binary knows. Stored in `PRAGMA user_version`. A database with a higher
/// value is refused with [`StoreError::SchemaTooNew`] instead of being migrated down.
pub const SCHEMA_VERSION: i64 = 1;

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("run {0} not found")]
    NotFound(RunId),
    /// Compare-and-set failed: the run was not in the expected status.
    #[error("conflict on run {run}: expected {expected:?}, found {actual:?}")]
    Conflict {
        run: RunId,
        expected: RunStatus,
        actual: RunStatus,
    },
    /// `RunStatus::can_transition_to` rejected the move.
    #[error("illegal transition {from:?} -> {to:?}")]
    IllegalTransition { from: RunStatus, to: RunStatus },
    #[error(
        "database schema version {found} is newer than this binary supports ({supported}); upgrade kestrel"
    )]
    SchemaTooNew { found: i64, supported: i64 },
}

/// One row of the `runs` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: RunId,
    pub agent: String,
    pub input: String,
    pub status: RunStatus,
    pub created_at: OffsetDateTime,
}

/// Filter for [`Store::list_runs`]. `Default` lists every run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFilter {
    pub agent: Option<String>,
    pub status: Option<RunStatus>,
    pub limit: Option<usize>,
}

/// Command sent from the CLI to the engine through the control table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlCommand {
    Pause,
    Resume,
    Cancel,
    Approve(ApprovalId),
    Reject(ApprovalId),
    SetBudget(MicroUsd),
}

/// Handle on the SQLite store. Shareable across threads (`Send + Sync`).
pub struct Store {
    _private: (),
}

impl Store {
    /// Opens (creating if needed) a file-backed store, enables WAL and runs migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let _ = path;
        todo!()
    }

    /// Opens a private in-memory store and runs migrations.
    pub fn open_in_memory() -> Result<Store> {
        todo!()
    }

    // runs

    /// Creates a run in `Queued`. Writes no event, so the run's first appended event gets seq 1.
    pub fn create_run(&self, agent: &AgentSpec, input: &str) -> Result<RunId> {
        let _ = (agent, input);
        todo!()
    }

    pub fn get_run(&self, id: RunId) -> Result<Option<RunRow>> {
        let _ = id;
        todo!()
    }

    pub fn list_runs(&self, filter: RunFilter) -> Result<Vec<RunRow>> {
        let _ = filter;
        todo!()
    }

    /// Compare-and-set on the run status. Checks `RunStatus::can_transition_to` and writes a
    /// `StatusChanged` event in the same SQL transaction.
    pub fn transition(&self, id: RunId, expected: &RunStatus, next: RunStatus) -> Result<()> {
        let _ = (id, expected, next);
        todo!()
    }

    // events

    /// Appends an event and assigns the next `seq` for the run (starting at 1).
    pub fn append(&self, run: RunId, kind: EventKind) -> Result<Event> {
        let _ = (run, kind);
        todo!()
    }

    /// Events of `run` with `seq > after_seq`, in `seq` order.
    pub fn events_since(&self, run: RunId, after_seq: u64) -> Result<Vec<Event>> {
        let _ = (run, after_seq);
        todo!()
    }

    // model calls and spend

    pub fn record_model_call(&self, rec: &ModelCallRecord) -> Result<()> {
        let _ = rec;
        todo!()
    }

    /// Sum of the known costs of a run's model calls (`cost: None` counts as zero).
    pub fn spent(&self, run: RunId) -> Result<MicroUsd> {
        let _ = run;
        todo!()
    }

    // control channel (CLI -> engine)

    pub fn push_control(&self, run: RunId, cmd: ControlCommand) -> Result<i64> {
        let _ = (run, cmd);
        todo!()
    }

    /// Claims every unclaimed command. Each command is returned by exactly one call, even with
    /// several consumers racing.
    pub fn take_control(&self) -> Result<Vec<(i64, RunId, ControlCommand)>> {
        todo!()
    }
}
