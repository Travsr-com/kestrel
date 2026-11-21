//! Append-only event log and run state on SQLite.
//!
//! In M1 there is no HTTP server: the CLI and the engine share one SQLite file (WAL mode) and talk
//! through it. Every write that must be atomic (seq assignment, status change plus its event,
//! claiming control commands) runs in a `BEGIN IMMEDIATE` transaction, so several handles and
//! processes on one file stay consistent.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use kestrel_types::{
    AgentSpec, ApprovalId, Event, EventKind, MicroUsd, ModelCallRecord, RunId, RunStatus,
};
use rusqlite::types::Type;
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Schema version this binary knows. Stored in `PRAGMA user_version`. A database with a higher
/// value is refused with [`StoreError::SchemaTooNew`] instead of being migrated down.
pub const SCHEMA_VERSION: i64 = 4;

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
    /// The OS random source failed while making a run token.
    #[error("no randomness for a run token: {0}")]
    Random(String),
    /// [`Store::spawn_child`] refused the spawn. Nothing was written.
    #[error("spawn refused: {0}")]
    Spawn(SpawnRefusal),
    /// The capability token for a spawned child could not be made. Nothing was written.
    #[error("capability token: {0}")]
    Capability(String),
    /// [`Store::set_budget`] would raise a spawned run by `asked`, and its parent has only
    /// `remaining` to give. Nothing was written.
    #[error("the parent has {remaining} left, and the raise needs {asked}")]
    ParentBudget {
        asked: MicroUsd,
        remaining: MicroUsd,
    },
}

/// Why [`Store::spawn_child`] refused a spawn.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpawnRefusal {
    /// Only a `Running` parent can spawn.
    #[error("parent {0} is {1:?}, not Running")]
    ParentNotRunning(RunId, RunStatus),
    /// The carve is not positive, or more than the parent has left.
    #[error("asked for {asked}, the parent has {remaining} left")]
    Budget {
        asked: MicroUsd,
        remaining: MicroUsd,
    },
    /// The parent already has this many children, live and finished.
    #[error("the parent already has {0} children, its limit")]
    TooManyChildren(u32),
    /// The tree already has this many runs.
    #[error("the tree already has {0} runs, its limit")]
    TooManyRuns(u32),
}

/// What [`Store::spawn_child`] needs to create a child.
#[derive(Debug, Clone)]
pub struct ChildPlan<'a> {
    /// The child's spec. Its `budget.limit` is replaced by the carve.
    pub spec: &'a AgentSpec,
    pub input: &'a str,
    /// The budget asked for. `None` takes the smaller of `spec.budget.limit` and half of what
    /// the parent has left.
    pub budget: Option<MicroUsd>,
    /// Most children the parent may have, live and finished.
    pub max_children: u32,
    /// Most runs the whole tree may have, its root included.
    pub max_tree_runs: u32,
    /// Written on the child right after its `RunCreated`.
    pub child_events: Vec<EventKind>,
}

/// A child made by [`Store::spawn_child`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawned {
    pub id: RunId,
    /// The budget carved out of the parent for it.
    pub budget: MicroUsd,
}

/// One row of the `runs` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: RunId,
    pub agent: String,
    pub input: String,
    pub status: RunStatus,
    pub created_at: OffsetDateTime,
    /// The run that spawned this one. `None` for a root.
    pub parent: Option<RunId>,
    /// The top of this run's tree. A root is its own root.
    pub root: RunId,
    /// Steps up to the root. 0 for a root.
    pub depth: u32,
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
    conn: Mutex<Connection>,
}

/// How long a writer waits for another handle's lock before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

const SCHEMA_V1: &str = "
CREATE TABLE runs (
    id         TEXT PRIMARY KEY,
    agent      TEXT NOT NULL,
    input      TEXT NOT NULL,
    status     TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE events (
    run_id TEXT NOT NULL,
    seq    INTEGER NOT NULL,
    body   TEXT NOT NULL,
    UNIQUE (run_id, seq)
);
CREATE TABLE model_calls (
    id     INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL,
    cost   INTEGER,
    body   TEXT NOT NULL
);
CREATE INDEX model_calls_run ON model_calls (run_id);
CREATE TABLE control (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id  TEXT NOT NULL,
    cmd     TEXT NOT NULL,
    claimed INTEGER NOT NULL DEFAULT 0
);
";

/// v2: the full agent spec of each run, as JSON, so a later engine can resume the run. A side
/// table rather than a new `runs` column, so `runs` keeps its v1 shape.
const SCHEMA_V2: &str = "
CREATE TABLE run_specs (
    run_id TEXT PRIMARY KEY,
    spec   TEXT NOT NULL
);
";

/// v3: a random token per run. The gateway URL of a run carries it, so run ids (visible in
/// listings and logs) never grant gateway access.
const SCHEMA_V3: &str = "
CREATE TABLE run_tokens (
    token  TEXT PRIMARY KEY,
    run_id TEXT NOT NULL UNIQUE
);
";

/// v4: the run tree. Each run knows its parent, its root and its depth, so a spawned child can be
/// found from either end. Runs from before v4 become roots: no parent, their own root, depth 0.
/// `root_run_id` is nullable only because SQLite cannot add a `NOT NULL` column without a default.
/// The `runs_default_root` trigger gives every row one anyway: an older binary still writing to a
/// migrated file inserts only the v3 columns, and its run must read back as a root.
/// `run_caps` holds one capability token per run; nothing writes it yet. `events_run` is keyed
/// `(run_id, rowid)`, so reading a tree's events after a cursor seeks instead of scanning.
const SCHEMA_V4: &str = "
ALTER TABLE runs ADD COLUMN parent_run_id TEXT;
ALTER TABLE runs ADD COLUMN root_run_id TEXT;
ALTER TABLE runs ADD COLUMN depth INTEGER NOT NULL DEFAULT 0;
UPDATE runs SET root_run_id = id;
CREATE TRIGGER runs_default_root AFTER INSERT ON runs WHEN NEW.root_run_id IS NULL
BEGIN
    UPDATE runs SET root_run_id = NEW.id WHERE rowid = NEW.rowid;
END;
CREATE TABLE run_caps (
    run_id TEXT PRIMARY KEY,
    token  BLOB NOT NULL
);
CREATE INDEX runs_parent ON runs (parent_run_id);
CREATE INDEX runs_root ON runs (root_run_id);
CREATE INDEX events_run ON events (run_id);
";

/// RFC 3339 timestamp, stored as JSON text like the rest of the typed columns.
#[derive(Serialize, Deserialize)]
struct Ts(#[serde(with = "time::serde::rfc3339")] OffsetDateTime);

/// A `runs` row as stored, before the JSON columns are decoded.
struct RawRun {
    id: RunId,
    agent: String,
    input: String,
    status: String,
    created_at: String,
    parent: Option<RunId>,
    root: RunId,
    depth: u32,
}

const RUN_COLUMNS: &str = "id, agent, input, status, created_at, parent_run_id, root_run_id, depth";

/// What a new child needs to know about its parent.
struct Parent {
    id: RunId,
    root: RunId,
    depth: u32,
}

impl Store {
    /// Opens (creating if needed) a file-backed store, enables WAL and runs migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        Self::init(Connection::open(path)?, true)
    }

    /// Opens a private in-memory store and runs migrations.
    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?, false)
    }

    fn init(mut conn: Connection, wal: bool) -> Result<Store> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        check_version(&conn)?;
        if wal {
            enable_wal(&conn)?;
        }
        migrate(&mut conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// A poisoned lock only means another thread panicked mid-call; any open transaction was
    /// rolled back on drop, so the connection is still usable.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // runs

    /// Creates a root run in `Queued` (no parent, its own root, depth 0) and stores its full spec
    /// (see [`Store::run_spec`]). Writes no event, so the run's first appended event gets seq 1.
    pub fn create_run(&self, agent: &AgentSpec, input: &str) -> Result<RunId> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = insert_run(&tx, agent, input, None)?;
        tx.commit()?;
        Ok(id)
    }

    /// Creates a child of `parent` in `Queued`: same root as the parent, depth one more. Stores
    /// the spec and a fresh token like [`Store::create_run`], and writes no event.
    /// [`StoreError::NotFound`] (and nothing written) when `parent` does not exist.
    pub fn create_child_run(&self, parent: RunId, agent: &AgentSpec, input: &str) -> Result<RunId> {
        let mut conn = self.conn();
        // The parent is read under the write lock, so it cannot change before the insert.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = tx
            .query_row(
                "SELECT root_run_id, depth FROM runs WHERE id = ?1",
                [parent.to_string()],
                |r| Ok((run_id_col(r, 0)?, r.get::<_, u32>(1)?)),
            )
            .optional()?;
        let Some((root, depth)) = found else {
            return Err(StoreError::NotFound(parent));
        };
        let above = Parent {
            id: parent,
            root,
            depth,
        };
        let id = insert_run(&tx, agent, input, Some(above))?;
        tx.commit()?;
        Ok(id)
    }

    /// Sets a run's budget limit (`kestrel budget`) in its stored spec, so the change survives a
    /// restart, and returns the limit set.
    ///
    /// The money of a tree comes from its root, so the limits of a tree keep adding up to the
    /// root's budget:
    ///
    /// * A root's limit is its own share (what it has not carved out for children), set as given.
    /// * A spawned run's raise is taken from its parent, refused with
    ///   [`StoreError::ParentBudget`] when the parent has less than that left. Lowering one gives
    ///   the difference back to its parent (or further up, like an ended run's unspent budget).
    /// * No run is lowered below what it already spent. A run that already spent more than its
    ///   limit keeps that limit when lowered, and gets exactly what was asked when raised.
    ///
    /// [`StoreError::NotFound`] when the run has no stored spec.
    pub fn set_budget(&self, id: RunId, limit: MicroUsd) -> Result<MicroUsd> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = read_limit(&tx, id)?;
        let spent = spent_in(&tx, id)?;
        // What was asked, raised only to what it spent and never past its old limit that way: a
        // lowering never takes money from the parent, and a raise gets no more than asked.
        let new = limit.max(spent.min(old));
        if let Some(parent) = parent_of(&tx, id)? {
            if new > old {
                let asked = MicroUsd(new.0 - old.0);
                let theirs = read_limit(&tx, parent)?;
                let remaining = left(theirs, spent_in(&tx, parent)?);
                if asked > remaining {
                    return Err(StoreError::ParentBudget { asked, remaining });
                }
                write_limit(&tx, parent, MicroUsd(theirs.0 - asked.0))?;
            } else if new < old {
                credit(&tx, parent, MicroUsd(old.0 - new.0))?;
            }
        }
        write_limit(&tx, id, new)?;
        tx.commit()?;
        Ok(new)
    }

    /// How many children of `run` have not ended yet.
    pub fn live_children(&self, run: RunId) -> Result<u64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        count(
            &tx,
            "SELECT COUNT(*) FROM runs WHERE parent_run_id = ?1
             AND status NOT IN ('\"Completed\"', '\"Cancelled\"')
             AND status NOT LIKE '{\"Failed\"%'",
            run,
        )
    }

    /// Creates a child of `parent` and carves its budget out of the parent's, in one transaction.
    ///
    /// A run may spend up to its own limit (`budget.limit` in its stored spec). The spawn moves
    /// the carve from the parent's limit to the child's, so the limits of a tree always add up to
    /// the root's budget (which only [`Store::set_budget`] on the root changes). A run can still go
    /// past its limit by the model calls it has in flight. When the child ends, its unspent budget
    /// goes back (see [`Store::transition`]).
    ///
    /// Refused with [`StoreError::Spawn`] when the parent is not `Running`, has
    /// `max_children` children already, its tree has `max_tree_runs` runs, or the carve is not
    /// positive or more than the parent has left (its limit minus its own spend).
    /// [`StoreError::NotFound`] when the parent does not exist.
    ///
    /// `cap` gets the child's id and carve and returns its capability token, stored in
    /// `run_caps` (`None` stores none). Events: the child's `RunCreated` naming the parent, then
    /// `plan.child_events`, then `ChildSpawned` on the parent. On any error nothing is written.
    pub fn spawn_child<F>(&self, parent: RunId, plan: ChildPlan<'_>, cap: F) -> Result<Spawned>
    where
        F: FnOnce(RunId, MicroUsd) -> Result<Option<Vec<u8>>>,
    {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = tx
            .query_row(
                "SELECT status, root_run_id, depth FROM runs WHERE id = ?1",
                [parent.to_string()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        run_id_col(r, 1)?,
                        r.get::<_, u32>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((status, root, depth)) = found else {
            return Err(StoreError::NotFound(parent));
        };
        let status: RunStatus = serde_json::from_str(&status)?;
        if status != RunStatus::Running {
            return Err(StoreError::Spawn(SpawnRefusal::ParentNotRunning(
                parent, status,
            )));
        }
        let children = count(
            &tx,
            "SELECT COUNT(*) FROM runs WHERE parent_run_id = ?1",
            parent,
        )?;
        if children >= u64::from(plan.max_children) {
            return Err(StoreError::Spawn(SpawnRefusal::TooManyChildren(
                plan.max_children,
            )));
        }
        let tree = count(
            &tx,
            "SELECT COUNT(*) FROM runs WHERE root_run_id = ?1",
            root,
        )?;
        if tree >= u64::from(plan.max_tree_runs) {
            return Err(StoreError::Spawn(SpawnRefusal::TooManyRuns(
                plan.max_tree_runs,
            )));
        }
        let limit = read_limit(&tx, parent)?;
        let remaining = left(limit, spent_in(&tx, parent)?);
        let carve = plan
            .budget
            .unwrap_or_else(|| plan.spec.budget.limit.min(MicroUsd(remaining.0 / 2)));
        if carve.0 <= 0 || carve > remaining {
            return Err(StoreError::Spawn(SpawnRefusal::Budget {
                asked: carve,
                remaining,
            }));
        }
        let mut spec = plan.spec.clone();
        spec.budget.limit = carve;
        let above = Parent {
            id: parent,
            root,
            depth,
        };
        let id = insert_run(&tx, &spec, plan.input, Some(above))?;
        write_limit(&tx, parent, MicroUsd(limit.0 - carve.0))?;
        if let Some(token) = cap(id, carve)? {
            tx.execute(
                "INSERT INTO run_caps (run_id, token) VALUES (?1, ?2)",
                params![id.to_string(), token],
            )?;
        }
        insert_event(
            &tx,
            id,
            EventKind::RunCreated {
                agent: spec.name.clone(),
                parent: Some(parent),
            },
        )?;
        for kind in plan.child_events {
            insert_event(&tx, id, kind)?;
        }
        insert_event(
            &tx,
            parent,
            EventKind::ChildSpawned {
                child: id,
                agent: spec.name,
                budget: carve,
            },
        )?;
        tx.commit()?;
        Ok(Spawned { id, budget: carve })
    }

    /// The run's budget limit: what it may spend itself. For a spawned run this moves: it shrinks
    /// by each carve for a child and grows when a child returns unspent budget.
    /// [`StoreError::NotFound`] when the run has no stored spec.
    pub fn limit(&self, run: RunId) -> Result<MicroUsd> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        read_limit(&tx, run)
    }

    /// What the run may still spend: its limit minus its own spend, never below zero.
    /// [`StoreError::NotFound`] when the run has no stored spec.
    pub fn remaining(&self, run: RunId) -> Result<MicroUsd> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        Ok(left(read_limit(&tx, run)?, spent_in(&tx, run)?))
    }

    /// The run's capability token, if it has one.
    pub fn run_cap(&self, run: RunId) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT token FROM run_caps WHERE run_id = ?1",
                [run.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Stores (or replaces) the run's capability token. [`StoreError::NotFound`] for an unknown run.
    pub fn set_run_cap(&self, run: RunId, token: &[u8]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run)?;
        tx.execute(
            "INSERT OR REPLACE INTO run_caps (run_id, token) VALUES (?1, ?2)",
            params![run.to_string(), token],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The run's gateway token. `None` for an unknown run or one created before schema v3.
    pub fn run_token(&self, id: RunId) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT token FROM run_tokens WHERE run_id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The run a gateway token belongs to, if any.
    pub fn run_by_token(&self, token: &str) -> Result<Option<RunId>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT run_id FROM run_tokens WHERE token = ?1",
                [token],
                |r| run_id_col(r, 0),
            )
            .optional()?)
    }

    /// The spec a run was created with. `None` for an unknown run, or for a run created before
    /// schema v2 (those have no stored spec).
    pub fn run_spec(&self, id: RunId) -> Result<Option<AgentSpec>> {
        let spec: Option<String> = self
            .conn()
            .query_row(
                "SELECT spec FROM run_specs WHERE run_id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(spec.map(|s| serde_json::from_str(&s)).transpose()?)
    }

    pub fn get_run(&self, id: RunId) -> Result<Option<RunRow>> {
        let raw = self
            .conn()
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                [id.to_string()],
                raw_run,
            )
            .optional()?;
        raw.map(decode_run).transpose()
    }

    pub fn list_runs(&self, filter: RunFilter) -> Result<Vec<RunRow>> {
        let status = filter
            .status
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        // A negative LIMIT means no limit in SQLite.
        let limit = filter
            .limit
            .map_or(-1, |l| i64::try_from(l).unwrap_or(i64::MAX));
        query_runs(
            &self.conn(),
            &format!(
                "SELECT {RUN_COLUMNS} FROM runs
                 WHERE (?1 IS NULL OR agent = ?1) AND (?2 IS NULL OR status = ?2)
                 ORDER BY rowid LIMIT ?3"
            ),
            params![filter.agent, status, limit],
        )
    }

    // run tree

    /// Direct children of `run`, in creation order. Empty for an unknown run.
    pub fn children(&self, run: RunId) -> Result<Vec<RunRow>> {
        query_runs(
            &self.conn(),
            &format!("SELECT {RUN_COLUMNS} FROM runs WHERE parent_run_id = ?1 ORDER BY rowid"),
            [run.to_string()],
        )
    }

    /// Every descendant of `run` (not `run` itself), each parent before its children: level by
    /// level, in creation order within a level. Works for any run, not only a root. Empty for an
    /// unknown run.
    pub fn subtree(&self, run: RunId) -> Result<Vec<RunRow>> {
        // UNION, not UNION ALL: a row is visited once, so the walk always ends.
        query_runs(
            &self.conn(),
            &format!(
                "WITH RECURSIVE below (run_id) AS (
                     SELECT id FROM runs WHERE parent_run_id = ?1
                     UNION
                     SELECT runs.id FROM runs JOIN below ON runs.parent_run_id = below.run_id
                 )
                 SELECT {RUN_COLUMNS} FROM runs WHERE id IN (SELECT run_id FROM below)
                 ORDER BY depth, rowid"
            ),
            [run.to_string()],
        )
    }

    /// The chain above `run`: its parent first, the root last. Empty for a root or an unknown run.
    pub fn ancestors(&self, run: RunId) -> Result<Vec<RunRow>> {
        query_runs(
            &self.conn(),
            &format!(
                "WITH RECURSIVE above (run_id) AS (
                     SELECT parent_run_id FROM runs WHERE id = ?1
                     UNION
                     SELECT runs.parent_run_id FROM runs JOIN above ON runs.id = above.run_id
                 )
                 SELECT {RUN_COLUMNS} FROM runs WHERE id IN (SELECT run_id FROM above)
                 ORDER BY depth DESC"
            ),
            [run.to_string()],
        )
    }

    /// Compare-and-set on the run status. Checks `RunStatus::can_transition_to` and writes a
    /// `StatusChanged` event in the same SQL transaction. When a spawned run ends, its unspent
    /// budget goes back to its parent in that transaction too (see [`Store::spawn_child`]).
    pub fn transition(&self, id: RunId, expected: &RunStatus, next: RunStatus) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual: Option<String> = tx
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let actual: RunStatus = match actual {
            Some(s) => serde_json::from_str(&s)?,
            None => return Err(StoreError::NotFound(id)),
        };
        if &actual != expected {
            return Err(StoreError::Conflict {
                run: id,
                expected: expected.clone(),
                actual,
            });
        }
        if !actual.can_transition_to(&next) {
            return Err(StoreError::IllegalTransition {
                from: actual,
                to: next,
            });
        }
        tx.execute(
            "UPDATE runs SET status = ?1 WHERE id = ?2",
            params![serde_json::to_string(&next)?, id.to_string()],
        )?;
        let ended = next.is_terminal();
        insert_event(
            &tx,
            id,
            EventKind::StatusChanged {
                from: actual,
                to: next,
            },
        )?;
        if ended {
            return_unspent(&tx, id)?;
        }
        tx.commit()?;
        Ok(())
    }

    // events

    /// Appends an event and assigns the next `seq` for the run (starting at 1).
    pub fn append(&self, run: RunId, kind: EventKind) -> Result<Event> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run)?;
        let event = insert_event(&tx, run, kind)?;
        tx.commit()?;
        Ok(event)
    }

    /// Events of `run` with `seq > after_seq`, in `seq` order.
    pub fn events_since(&self, run: RunId, after_seq: u64) -> Result<Vec<Event>> {
        let after = i64::try_from(after_seq).unwrap_or(i64::MAX);
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT body FROM events WHERE run_id = ?1 AND seq > ?2 ORDER BY seq")?;
        let bodies = stmt
            .query_map(params![run.to_string(), after], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        bodies
            .iter()
            .map(|b| serde_json::from_str(b).map_err(StoreError::from))
            .collect()
    }

    /// Events of every run whose root is `root` (the root's own included) with a cursor greater
    /// than `after_cursor`, in commit order. The cursor is the `events` rowid; 0 reads from the
    /// start. It relies on `events` being append-only: rows are never deleted, so the implicit
    /// rowid only grows. `root` must be a root run (see [`RunRow::root`]): an unknown id, or the
    /// id of a run that is not a root, reads nothing.
    pub fn events_since_global(&self, root: RunId, after_cursor: u64) -> Result<Vec<(u64, Event)>> {
        let after = i64::try_from(after_cursor).unwrap_or(i64::MAX);
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT rowid, body FROM events
             WHERE rowid > ?2 AND run_id IN (SELECT id FROM runs WHERE root_run_id = ?1)
             ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![root.to_string(), after], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            // Rowids start at 1, so the u64 view is exact.
            .map(|(rowid, body)| Ok((rowid.unsigned_abs(), serde_json::from_str(body)?)))
            .collect()
    }

    // model calls and spend

    pub fn record_model_call(&self, rec: &ModelCallRecord) -> Result<()> {
        self.conn().execute(
            "INSERT INTO model_calls (run_id, cost, body) VALUES (?1, ?2, ?3)",
            params![
                rec.run_id.to_string(),
                rec.cost.map(|c| c.0),
                serde_json::to_string(rec)?,
            ],
        )?;
        Ok(())
    }

    /// Sum of the known costs of a run's model calls (`cost: None` counts as zero).
    pub fn spent(&self, run: RunId) -> Result<MicroUsd> {
        // SUM over INTEGER stays integer and skips NULLs; overflow is an error, not a wrap.
        let total: i64 = self.conn().query_row(
            "SELECT COALESCE(SUM(cost), 0) FROM model_calls WHERE run_id = ?1",
            [run.to_string()],
            |r| r.get(0),
        )?;
        Ok(MicroUsd(total))
    }

    // control channel (CLI -> engine)

    pub fn push_control(&self, run: RunId, cmd: ControlCommand) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run)?;
        tx.execute(
            "INSERT INTO control (run_id, cmd) VALUES (?1, ?2)",
            params![run.to_string(), serde_json::to_string(&cmd)?],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Claims every unclaimed command. Each command is returned by exactly one call, even with
    /// several consumers racing.
    pub fn take_control(&self) -> Result<Vec<(i64, RunId, ControlCommand)>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raws = {
            let mut stmt = tx.prepare(
                "UPDATE control SET claimed = 1 WHERE claimed = 0 RETURNING id, run_id, cmd",
            )?;
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    run_id_col(r, 1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.commit()?;
        let mut cmds = raws
            .into_iter()
            .map(|(id, run, cmd)| Ok((id, run, serde_json::from_str(&cmd)?)))
            .collect::<Result<Vec<_>>>()?;
        // RETURNING order is unspecified; callers get commands in push order.
        cmds.sort_by_key(|c| c.0);
        Ok(cmds)
    }
}

/// Refuses a database written by a newer binary before anything touches it.
fn check_version(conn: &Connection) -> Result<i64> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(found)
}

/// Switches a file to WAL. SQLite does not call the busy handler for this switch, so when several
/// processes open a brand-new file at once it can fail with `SQLITE_BUSY` straight away. Retry
/// with a short backoff, bounded by [`BUSY_TIMEOUT`].
fn enable_wal(conn: &Connection) -> Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    let mut backoff = Duration::from_millis(1);
    loop {
        match switch_to_wal(conn) {
            Err(e) if is_busy(&e) && Instant::now() < deadline => {
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            other => return Ok(other?),
        }
    }
}

/// Reads the mode first: an already-WAL file (every open after the first) needs no lock.
fn switch_to_wal(conn: &Connection) -> rusqlite::Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        // Returns the resulting mode as a row, so it is read rather than executed.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    }
    Ok(())
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// A `COUNT(*)` query with the run id as its one parameter.
fn count(tx: &Transaction<'_>, sql: &str, run: RunId) -> Result<u64> {
    let n: i64 = tx.query_row(sql, [run.to_string()], |r| r.get(0))?;
    Ok(n.unsigned_abs())
}

/// What is left of `limit` after `spent`, never below zero.
fn left(limit: MicroUsd, spent: MicroUsd) -> MicroUsd {
    MicroUsd(limit.0.saturating_sub(spent.0).max(0))
}

fn spent_in(tx: &Transaction<'_>, run: RunId) -> Result<MicroUsd> {
    let total: i64 = tx.query_row(
        "SELECT COALESCE(SUM(cost), 0) FROM model_calls WHERE run_id = ?1",
        [run.to_string()],
        |r| r.get(0),
    )?;
    Ok(MicroUsd(total))
}

fn read_spec(tx: &Transaction<'_>, run: RunId) -> Result<AgentSpec> {
    let spec: Option<String> = tx
        .query_row(
            "SELECT spec FROM run_specs WHERE run_id = ?1",
            [run.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    match spec {
        Some(s) => Ok(serde_json::from_str(&s)?),
        None => Err(StoreError::NotFound(run)),
    }
}

fn read_limit(tx: &Transaction<'_>, run: RunId) -> Result<MicroUsd> {
    Ok(read_spec(tx, run)?.budget.limit)
}

fn write_limit(tx: &Transaction<'_>, run: RunId, limit: MicroUsd) -> Result<()> {
    let mut spec = read_spec(tx, run)?;
    spec.budget.limit = limit;
    tx.execute(
        "UPDATE run_specs SET spec = ?1 WHERE run_id = ?2",
        params![serde_json::to_string(&spec)?, run.to_string()],
    )?;
    Ok(())
}

fn parent_of(tx: &Transaction<'_>, run: RunId) -> Result<Option<RunId>> {
    Ok(tx.query_row(
        "SELECT parent_run_id FROM runs WHERE id = ?1",
        [run.to_string()],
        |r| opt_run_id_col(r, 0),
    )?)
}

fn status_of(tx: &Transaction<'_>, run: RunId) -> Result<RunStatus> {
    let status: String = tx.query_row(
        "SELECT status FROM runs WHERE id = ?1",
        [run.to_string()],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&status)?)
}

/// Gives a spawned run that just ended its unspent budget back to its parent. The run keeps a
/// limit equal to what it spent, so a second call returns nothing. A root keeps its own limit:
/// there is no one to return it to. A run that spent more than its limit returns nothing and
/// takes nothing.
fn return_unspent(tx: &Transaction<'_>, run: RunId) -> Result<()> {
    let Some(parent) = parent_of(tx, run)? else {
        return Ok(());
    };
    let limit = read_limit(tx, run)?;
    let spent = spent_in(tx, run)?;
    let amount = left(limit, spent);
    if amount.0 == 0 {
        return Ok(());
    }
    write_limit(tx, run, spent)?;
    credit(tx, parent, amount)
}

/// Adds `amount` to `parent`'s limit. When `parent` has ended, it keeps only what it spent and
/// passes the rest on up to the nearest run still going, or to the root, which keeps it.
fn credit(tx: &Transaction<'_>, mut parent: RunId, mut amount: MicroUsd) -> Result<()> {
    loop {
        let to = read_limit(tx, parent)?;
        let grandparent = parent_of(tx, parent)?;
        let ended = status_of(tx, parent)?.is_terminal();
        let Some(next) = grandparent.filter(|_| ended) else {
            // Still going, or a root: it keeps the amount.
            return write_limit(tx, parent, MicroUsd(to.0.saturating_add(amount.0)));
        };
        // An ended run in the middle passes the amount on, keeping only what it spent.
        let total = MicroUsd(to.0.saturating_add(amount.0));
        let spent = spent_in(tx, parent)?;
        amount = left(total, spent);
        write_limit(tx, parent, if amount.0 > 0 { spent } else { total })?;
        if amount.0 == 0 {
            return Ok(());
        }
        parent = next;
    }
}

/// Fails with [`StoreError::NotFound`] unless `run` exists, so no orphan rows are written.
fn ensure_run(tx: &Transaction<'_>, run: RunId) -> Result<()> {
    let found = tx
        .query_row(
            "SELECT 1 FROM runs WHERE id = ?1",
            [run.to_string()],
            |_| Ok(()),
        )
        .optional()?;
    found.ok_or(StoreError::NotFound(run))
}

/// Brings the schema up to [`SCHEMA_VERSION`]. A no-op when it is already there.
fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Re-read under the write lock: another handle may have migrated since the first check.
    let found = check_version(&tx)?;
    if found < 1 {
        tx.execute_batch(SCHEMA_V1)?;
    }
    if found < 2 {
        tx.execute_batch(SCHEMA_V2)?;
    }
    if found < 3 {
        tx.execute_batch(SCHEMA_V3)?;
    }
    if found < 4 {
        tx.execute_batch(SCHEMA_V4)?;
    }
    if found < SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

/// 128 random bits from the OS, as 32 lowercase hex characters.
fn new_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| StoreError::Random(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Inserts a `Queued` run with its spec and a fresh gateway token. `above` places it under a
/// parent; `None` makes a root. Callers hold an IMMEDIATE transaction.
fn insert_run(
    tx: &Transaction<'_>,
    agent: &AgentSpec,
    input: &str,
    above: Option<Parent>,
) -> Result<RunId> {
    let id = RunId::new();
    let (parent, root, depth) = match above {
        Some(p) => (Some(p.id), p.root, p.depth.saturating_add(1)),
        None => (None, id, 0),
    };
    tx.execute(
        &format!("INSERT INTO runs ({RUN_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
        params![
            id.to_string(),
            agent.name,
            input,
            serde_json::to_string(&RunStatus::Queued)?,
            serde_json::to_string(&Ts(OffsetDateTime::now_utc()))?,
            parent.map(|p| p.to_string()),
            root.to_string(),
            depth,
        ],
    )?;
    tx.execute(
        "INSERT INTO run_specs (run_id, spec) VALUES (?1, ?2)",
        params![id.to_string(), serde_json::to_string(agent)?],
    )?;
    tx.execute(
        "INSERT INTO run_tokens (token, run_id) VALUES (?1, ?2)",
        params![new_token()?, id.to_string()],
    )?;
    Ok(id)
}

/// Appends one event with the run's next seq. Callers hold an IMMEDIATE transaction, so no other
/// writer can pick the same seq; `UNIQUE (run_id, seq)` backs that up.
fn insert_event(tx: &Transaction<'_>, run: RunId, kind: EventKind) -> Result<Event> {
    // Stored as i64 (SQLite INTEGER); MAX + 1 is always at least 1, so the u64 view is exact.
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE run_id = ?1",
        [run.to_string()],
        |r| r.get(0),
    )?;
    let event = Event {
        run_id: run,
        seq: seq.unsigned_abs(),
        ts: OffsetDateTime::now_utc(),
        kind,
    };
    tx.execute(
        "INSERT INTO events (run_id, seq, body) VALUES (?1, ?2, ?3)",
        params![run.to_string(), seq, serde_json::to_string(&event)?],
    )?;
    Ok(event)
}

fn run_id_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<RunId> {
    parse_run_id(idx, &row.get::<_, String>(idx)?)
}

/// Like [`run_id_col`] for a nullable column.
fn opt_run_id_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<RunId>> {
    row.get::<_, Option<String>>(idx)?
        .map(|text| parse_run_id(idx, &text))
        .transpose()
}

fn parse_run_id(idx: usize, text: &str) -> rusqlite::Result<RunId> {
    text.parse()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

/// Reads one row selected as [`RUN_COLUMNS`].
fn raw_run(row: &Row<'_>) -> rusqlite::Result<RawRun> {
    Ok(RawRun {
        id: run_id_col(row, 0)?,
        agent: row.get(1)?,
        input: row.get(2)?,
        status: row.get(3)?,
        created_at: row.get(4)?,
        parent: opt_run_id_col(row, 5)?,
        root: run_id_col(row, 6)?,
        depth: row.get(7)?,
    })
}

fn decode_run(raw: RawRun) -> Result<RunRow> {
    let Ts(created_at) = serde_json::from_str(&raw.created_at)?;
    Ok(RunRow {
        id: raw.id,
        agent: raw.agent,
        input: raw.input,
        status: serde_json::from_str(&raw.status)?,
        created_at,
        parent: raw.parent,
        root: raw.root,
        depth: raw.depth,
    })
}

/// Runs `sql`, a SELECT of [`RUN_COLUMNS`], and decodes every row.
fn query_runs(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(sql)?;
    let raws = stmt
        .query_map(params, raw_run)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(decode_run).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_YAML: &str = "name: a\nruntime: opencode\nrole: r\nmodel_policy: { default: tier:cheap }\n\
        sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: git-clone }\n\
        budget: { usd: 1.00, wall_clock: 10m }\n";

    fn agent(name: &str) -> AgentSpec {
        let yaml = AGENT_YAML.replace("name: a", &format!("name: {name}"));
        AgentSpec::from_yaml(&yaml).expect("fixture parses")
    }

    #[test]
    fn list_runs_filters_by_agent_status_and_limit() {
        let store = Store::open_in_memory().expect("open");
        let a1 = store.create_run(&agent("alpha"), "1").expect("create");
        let a2 = store.create_run(&agent("alpha"), "2").expect("create");
        let b1 = store.create_run(&agent("beta"), "3").expect("create");
        store
            .transition(a2, &RunStatus::Queued, RunStatus::Cancelled)
            .expect("cancel");

        let ids = |f: RunFilter| -> Vec<RunId> {
            store
                .list_runs(f)
                .expect("list")
                .into_iter()
                .map(|r| r.id)
                .collect()
        };
        assert_eq!(ids(RunFilter::default()), vec![a1, a2, b1]);
        let alpha = RunFilter {
            agent: Some("alpha".into()),
            ..RunFilter::default()
        };
        assert_eq!(ids(alpha.clone()), vec![a1, a2]);
        assert_eq!(
            ids(RunFilter {
                status: Some(RunStatus::Queued),
                ..alpha
            }),
            vec![a1]
        );
        assert_eq!(
            ids(RunFilter {
                limit: Some(2),
                ..RunFilter::default()
            }),
            vec![a1, a2]
        );
    }

    #[test]
    fn unknown_run_is_none_or_not_found() {
        let store = Store::open_in_memory().expect("open");
        let id = RunId::new();
        assert_eq!(store.get_run(id).expect("get_run"), None);
        match store.transition(id, &RunStatus::Queued, RunStatus::Provisioning) {
            Err(StoreError::NotFound(r)) => assert_eq!(r, id),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn writes_to_unknown_run_are_not_found_and_write_nothing() {
        let store = Store::open_in_memory().expect("open");
        let ghost = RunId::new();
        let msg = EventKind::AgentMessage { text: "x".into() };
        match store.append(ghost, msg) {
            Err(StoreError::NotFound(r)) => assert_eq!(r, ghost),
            other => panic!("expected NotFound, got {other:?}"),
        }
        match store.push_control(ghost, ControlCommand::Cancel) {
            Err(StoreError::NotFound(r)) => assert_eq!(r, ghost),
            other => panic!("expected NotFound, got {other:?}"),
        }
        let count = |table: &str| -> i64 {
            store
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .expect("count")
        };
        assert_eq!(count("events"), 0);
        assert_eq!(count("control"), 0);
    }

    #[test]
    fn busy_and_locked_are_retryable_other_errors_are_not() {
        let err = |code| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
        assert!(is_busy(&err(rusqlite::ffi::SQLITE_BUSY)));
        assert!(is_busy(&err(rusqlite::ffi::SQLITE_LOCKED)));
        assert!(!is_busy(&err(rusqlite::ffi::SQLITE_CORRUPT)));
        assert!(!is_busy(&rusqlite::Error::QueryReturnedNoRows));
    }

    #[test]
    fn corrupt_rows_are_errors_not_panics() {
        let store = Store::open_in_memory().expect("open");
        let run = RunId::new();
        {
            let conn = store.conn();
            conn.execute(
                "INSERT INTO runs (id, agent, input, status, created_at, root_run_id)
                 VALUES ('not-a-uuid', 'a', '', '\"Queued\"', '\"x\"', 'not-a-uuid')",
                [],
            )
            .expect("insert run");
            conn.execute("INSERT INTO events VALUES (?1, 1, '{')", [run.to_string()])
                .expect("insert event");
            conn.execute(
                "INSERT INTO control (run_id, cmd) VALUES (?1, '\"Nope\"')",
                [run.to_string()],
            )
            .expect("insert control");
        }
        assert!(matches!(
            store.list_runs(RunFilter::default()),
            Err(StoreError::Sqlite(_))
        ));
        assert!(matches!(
            store.events_since(run, 0),
            Err(StoreError::Serde(_))
        ));
        assert!(matches!(store.take_control(), Err(StoreError::Serde(_))));
    }
}
