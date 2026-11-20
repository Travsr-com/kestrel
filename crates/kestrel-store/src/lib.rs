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

    /// Changes the budget limit in a run's stored spec, so the change survives a restart.
    /// [`StoreError::NotFound`] when the run has no stored spec.
    pub fn set_budget(&self, id: RunId, limit: MicroUsd) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let spec: Option<String> = tx
            .query_row(
                "SELECT spec FROM run_specs WHERE run_id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let mut spec: AgentSpec = match spec {
            Some(s) => serde_json::from_str(&s)?,
            None => return Err(StoreError::NotFound(id)),
        };
        spec.budget.limit = limit;
        tx.execute(
            "UPDATE run_specs SET spec = ?1 WHERE run_id = ?2",
            params![serde_json::to_string(&spec)?, id.to_string()],
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
    /// `StatusChanged` event in the same SQL transaction.
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
        insert_event(
            &tx,
            id,
            EventKind::StatusChanged {
                from: actual,
                to: next,
            },
        )?;
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
