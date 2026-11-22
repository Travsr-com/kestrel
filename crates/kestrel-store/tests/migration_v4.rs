//! Issue #32, scenarios 1 to 3: the schema v4 migration (run tree columns, `run_caps`, indexes).
//!
//! `tests/fixtures/v3.db` is a real v3 database; `tests/fixtures/README.md` lists what it holds.
//! Opening a database migrates it, so every test works on a copy in a temp dir.

// Test helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use kestrel_store::{ControlCommand, RunFilter, RunRow, SCHEMA_VERSION, Store, StoreError};
use kestrel_types::{AgentSpec, EventKind, MicroUsd, RunId, RunStatus};
use rusqlite::types::Value;
use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// One run of the fixture, as listed in `tests/fixtures/README.md`.
struct FixtureRun {
    id: &'static str,
    agent: &'static str,
    input: &'static str,
    status: RunStatus,
    created_at: &'static str,
    token: &'static str,
    /// Rowids of this run's rows in `events`, in commit order.
    event_rowids: &'static [u64],
    spent: i64,
}

impl FixtureRun {
    fn id(&self) -> RunId {
        self.id.parse().expect("fixture run id parses")
    }

    fn created_at(&self) -> OffsetDateTime {
        OffsetDateTime::parse(self.created_at, &Rfc3339).expect("fixture timestamp parses")
    }
}

/// The three fixture runs, in creation order.
fn fixture_runs() -> [FixtureRun; 3] {
    [
        FixtureRun {
            id: "01a1f9a6-a0fa-713c-9285-84f322e2bdf0",
            agent: "alpha",
            input: "first task",
            status: RunStatus::Completed,
            created_at: "2026-11-20T15:50:09.402919Z",
            token: "6bf4513999e8cb561871c8618a5fd313",
            event_rowids: &[1, 3, 5, 6, 9, 10],
            spent: 2_000_000,
        },
        FixtureRun {
            id: "01a1f9a6-a0ff-77ec-a2d5-479a961d99ff",
            agent: "beta",
            input: "second task",
            status: RunStatus::Queued,
            created_at: "2026-11-20T15:50:09.4071576Z",
            token: "9fca38a322617a872d27ba1c5bc1d0aa",
            event_rowids: &[2, 7],
            spent: 500_000,
        },
        FixtureRun {
            id: "01a1f9a6-a102-7752-a215-cf40438bdab3",
            agent: "gamma",
            input: "third task",
            status: RunStatus::Cancelled,
            created_at: "2026-11-20T15:50:09.4103933Z",
            token: "f32edea3504095b94d1a1a6676bd6b50",
            event_rowids: &[4, 8],
            spent: 0,
        },
    ]
}

/// The spec every fixture run was created with, apart from the name.
fn agent(name: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: opencode\nrole: r\nmodel_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 1.00, wall_clock: 10m }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture parses")
}

/// Copies the checked-in v3 database into `dir` and returns the path of the copy.
fn fixture_copy(dir: &TempDir) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3.db");
    let copy = dir.path().join("kestrel.db");
    std::fs::copy(&source, &copy).expect("copy the v3 fixture");
    copy
}

/// Runs `sql` on a raw connection (not through the store) and returns every row.
fn rows(path: &Path, sql: &str) -> Vec<Vec<Value>> {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let mut stmt = conn.prepare(sql).expect("prepare");
    let columns = stmt.column_count();
    stmt.query_map([], |r| {
        (0..columns)
            .map(|i| r.get::<_, Value>(i))
            .collect::<rusqlite::Result<Vec<_>>>()
    })
    .expect("query")
    .collect::<rusqlite::Result<Vec<_>>>()
    .expect("rows")
}

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .expect("raw open")
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version")
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

/// Every v3 table, read by its v3 columns only, so the same queries work before and after the
/// migration. `events` includes the rowid, which is the global cursor (an alias of `events.cursor`
/// from v5 on).
const V3_DATA: [&str; 6] = [
    "SELECT id, agent, input, status, created_at FROM runs ORDER BY rowid",
    "SELECT rowid, run_id, seq, body FROM events ORDER BY rowid",
    "SELECT id, run_id, cost, body FROM model_calls ORDER BY id",
    "SELECT id, run_id, cmd, claimed FROM control ORDER BY id",
    "SELECT run_id, spec FROM run_specs ORDER BY run_id",
    "SELECT token, run_id FROM run_tokens ORDER BY run_id",
];

fn v3_data(path: &Path) -> Vec<Vec<Vec<Value>>> {
    V3_DATA.iter().map(|sql| rows(path, sql)).collect()
}

/// Everything in the file: version, schema, every row of every table, and the event rowids.
#[derive(Debug, PartialEq)]
struct Snapshot {
    version: i64,
    schema: Vec<Vec<Value>>,
    tables: Vec<(String, Vec<Vec<Value>>)>,
    event_rowids: Vec<Vec<Value>>,
}

fn snapshot(path: &Path) -> Snapshot {
    let names = rows(
        path,
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    );
    let tables = names
        .into_iter()
        .map(|row| match row.into_iter().next() {
            Some(Value::Text(name)) => {
                // Every table has at least two columns and is unique on its first two.
                let data = rows(path, &format!("SELECT * FROM \"{name}\" ORDER BY 1, 2"));
                (name, data)
            }
            other => panic!("table name is not text: {other:?}"),
        })
        .collect();
    Snapshot {
        version: user_version(path),
        schema: rows(
            path,
            "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name",
        ),
        tables,
        event_rowids: rows(path, "SELECT rowid, run_id, seq FROM events ORDER BY rowid"),
    }
}

/// `(name, declared type in upper case, NOT NULL, part of the primary key)` per column.
fn table_columns(path: &Path, table: &str) -> Vec<(String, String, bool, bool)> {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let mut stmt = conn
        .prepare("SELECT name, type, \"notnull\", pk FROM pragma_table_info(?1) ORDER BY cid")
        .expect("prepare");
    stmt.query_map([table], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?.to_ascii_uppercase(),
            r.get::<_, i64>(2)? != 0,
            r.get::<_, i64>(3)? != 0,
        ))
    })
    .expect("query")
    .collect::<rusqlite::Result<Vec<_>>>()
    .expect("rows")
}

/// The leading column of every index on `runs`: names from `sqlite_master`, columns from
/// `PRAGMA index_info`. The index names are the implementer's choice, the indexed columns are not.
fn runs_index_leading_columns(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'runs'")
        .expect("prepare");
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("index names");
    names
        .iter()
        .filter_map(|name| {
            conn.query_row(
                "SELECT name FROM pragma_index_info(?1) WHERE seqno = 0",
                [name],
                |r| r.get::<_, Option<String>>(0),
            )
            .expect("index_info")
        })
        .collect()
}

fn run(store: &Store, id: RunId) -> RunRow {
    store.get_run(id).expect("get_run").expect("run exists")
}

// 1. Given a v3 database with runs, when it is opened, then it migrates to v4 (and on to the current
//    version) and no data is lost. This part: every v3 row is byte for byte the same.
#[test]
fn s1_v3_database_migrates_to_v4_and_loses_no_data() {
    let dir = TempDir::new().expect("tempdir");
    let path = fixture_copy(&dir);
    assert_eq!(user_version(&path), 3, "the fixture must be a v3 database");
    let before = v3_data(&path);
    let counts: Vec<usize> = before.iter().map(Vec::len).collect();
    assert_eq!(
        counts,
        vec![3, 10, 4, 2, 3, 3],
        "fixture rows (runs, events, model_calls, control, run_specs, run_tokens)"
    );

    let store = Store::open(&path).expect("open migrates");
    assert_eq!(user_version(&path), SCHEMA_VERSION);
    assert_eq!(v3_data(&path), before, "migration changed existing rows");

    drop(store);
    assert_eq!(user_version(&path), SCHEMA_VERSION);
    assert_eq!(
        v3_data(&path),
        before,
        "rows changed after the store closed"
    );
}

// 1. ... existing runs get `root_run_id` equal to their own id, depth 0, no parent.
#[test]
fn s1_migrated_runs_are_their_own_root_at_depth_0_with_no_parent() {
    let dir = TempDir::new().expect("tempdir");
    let path = fixture_copy(&dir);
    let store = Store::open(&path).expect("open migrates");
    let fixture = fixture_runs();

    // The columns as stored.
    let stored = rows(
        &path,
        "SELECT id, parent_run_id, root_run_id, depth FROM runs ORDER BY rowid",
    );
    let want: Vec<Vec<Value>> = fixture
        .iter()
        .map(|f| vec![text(f.id), Value::Null, text(f.id), Value::Integer(0)])
        .collect();
    assert_eq!(stored, want);

    // The same through the store, with every v3 field intact.
    let want: Vec<RunRow> = fixture
        .iter()
        .map(|f| RunRow {
            id: f.id(),
            agent: f.agent.to_owned(),
            input: f.input.to_owned(),
            status: f.status.clone(),
            created_at: f.created_at(),
            parent: None,
            root: f.id(),
            depth: 0,
        })
        .collect();
    assert_eq!(store.list_runs(RunFilter::default()).expect("list"), want);
    for row in &want {
        assert_eq!(&run(&store, row.id), row);
        assert!(store.children(row.id).expect("children").is_empty());
        assert!(store.subtree(row.id).expect("subtree").is_empty());
        assert!(store.ancestors(row.id).expect("ancestors").is_empty());
    }
}

// 1. ... and no data is lost. This part: events, spend, specs, tokens and control commands read
//    back through the store with the values listed in the fixture README.
#[test]
fn s1_migrated_events_spend_specs_tokens_and_control_read_back() {
    let dir = TempDir::new().expect("tempdir");
    let path = fixture_copy(&dir);
    let store = Store::open(&path).expect("open migrates");
    let [alpha, beta, gamma] = fixture_runs();

    let kinds = |f: &FixtureRun| -> Vec<EventKind> {
        let events = store.events_since(f.id(), 0).expect("events_since");
        let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=events.len() as u64).collect::<Vec<_>>());
        assert!(events.iter().all(|e| e.run_id == f.id()));
        events.into_iter().map(|e| e.kind).collect()
    };
    let changed = |from: RunStatus, to: RunStatus| EventKind::StatusChanged { from, to };
    // A v3 `run_created` body has no `parent`; it decodes as `None`.
    let created = |f: &FixtureRun| EventKind::RunCreated {
        agent: f.agent.to_owned(),
        parent: None,
    };
    assert_eq!(
        kinds(&alpha),
        vec![
            created(&alpha),
            changed(RunStatus::Queued, RunStatus::Provisioning),
            changed(RunStatus::Provisioning, RunStatus::Running),
            EventKind::AgentMessage {
                text: "hello from alpha".into()
            },
            changed(RunStatus::Running, RunStatus::Completed),
            EventKind::RunFinished {
                summary: Some("done".into())
            },
        ]
    );
    assert_eq!(
        kinds(&beta),
        vec![
            created(&beta),
            EventKind::AgentMessage {
                text: "hello from beta".into()
            },
        ]
    );
    assert_eq!(
        kinds(&gamma),
        vec![
            created(&gamma),
            changed(RunStatus::Queued, RunStatus::Cancelled),
        ]
    );

    for f in [&alpha, &beta, &gamma] {
        let id = f.id();
        assert_eq!(store.spent(id).expect("spent"), MicroUsd(f.spent));
        assert_eq!(store.run_spec(id).expect("run_spec"), Some(agent(f.agent)));
        assert_eq!(
            store.run_token(id).expect("run_token").as_deref(),
            Some(f.token)
        );
        assert_eq!(store.run_by_token(f.token).expect("run_by_token"), Some(id));

        // Each old run is the root of a tree of one, so its global stream is its own events,
        // and the cursor is the `events` rowid the row had in v3.
        let global = store
            .events_since_global(id, 0)
            .expect("events_since_global");
        let cursors: Vec<u64> = global.iter().map(|(cursor, _)| *cursor).collect();
        assert_eq!(cursors, f.event_rowids);
        let events: Vec<_> = global.into_iter().map(|(_, event)| event).collect();
        assert_eq!(events, store.events_since(id, 0).expect("events_since"));
    }

    // Command 1 was already claimed in v3; only command 2 is still waiting.
    assert_eq!(
        store.take_control().expect("take_control"),
        vec![(2, beta.id(), ControlCommand::Pause)]
    );
    assert!(store.take_control().expect("take_control").is_empty());
}

// 1. The v4 migration also adds the `run_caps` table and an index on `parent_run_id` and on
//    `root_run_id`.
#[test]
fn s1_migration_adds_run_caps_table_and_both_run_tree_indexes() {
    let dir = TempDir::new().expect("tempdir");
    let path = fixture_copy(&dir);
    let alpha = fixture_runs()[0].id();
    drop(Store::open(&path).expect("open migrates"));

    let tables = rows(
        &path,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'run_caps'",
    );
    assert_eq!(tables, vec![vec![text("run_caps")]]);
    // run_caps (run_id TEXT PRIMARY KEY, token BLOB NOT NULL)
    let columns = table_columns(&path, "run_caps");
    let shape: Vec<(&str, &str, bool)> = columns
        .iter()
        .map(|(name, ty, _, pk)| (name.as_str(), ty.as_str(), *pk))
        .collect();
    assert_eq!(
        shape,
        vec![("run_id", "TEXT", true), ("token", "BLOB", false)]
    );
    assert!(columns[1].2, "run_caps.token must be NOT NULL");
    // Tokens are written by a later issue, so the migration leaves the table empty.
    assert!(rows(&path, "SELECT * FROM run_caps").is_empty());

    // The table behaves as declared: a blob per run, one row per run, no NULL token.
    let conn = rusqlite::Connection::open(&path).expect("raw open");
    let insert = "INSERT INTO run_caps (run_id, token) VALUES (?1, ?2)";
    let token: &[u8] = &[0x00, 0xff, 0x10, 0x80];
    conn.execute(insert, rusqlite::params![alpha.to_string(), token])
        .expect("insert a capability token");
    let stored: Value = conn
        .query_row(
            "SELECT token FROM run_caps WHERE run_id = ?1",
            [alpha.to_string()],
            |r| r.get(0),
        )
        .expect("read the token back");
    assert_eq!(stored, Value::Blob(token.to_vec()));
    assert!(
        conn.execute(insert, rusqlite::params![alpha.to_string(), token])
            .is_err(),
        "run_id is the primary key, a second token for one run must be refused"
    );
    assert!(
        conn.execute(
            insert,
            rusqlite::params![RunId::new().to_string(), Option::<Vec<u8>>::None]
        )
        .is_err(),
        "a NULL token must be refused"
    );
    drop(conn);

    let names: Vec<String> = table_columns(&path, "runs")
        .into_iter()
        .map(|(name, ..)| name)
        .collect();
    for column in ["parent_run_id", "root_run_id", "depth"] {
        assert!(
            names.iter().any(|n| n == column),
            "runs.{column} missing: {names:?}"
        );
    }
    let indexed = runs_index_leading_columns(&path);
    for column in ["parent_run_id", "root_run_id"] {
        assert!(
            indexed.iter().any(|c| c == column),
            "no index on runs leads with {column}: {indexed:?}"
        );
    }
}

// 1. A migrated database keeps working: old runs can get children, and new roots can be made.
#[test]
fn s1_migrated_database_accepts_new_roots_and_children_of_old_runs() {
    let dir = TempDir::new().expect("tempdir");
    let path = fixture_copy(&dir);
    let store = Store::open(&path).expect("open migrates");
    let [alpha, beta, _] = fixture_runs();

    let child = store
        .create_child_run(alpha.id(), &agent("child"), "child task")
        .expect("create_child_run");
    let row = run(&store, child);
    assert_eq!(
        (row.parent, row.root, row.depth),
        (Some(alpha.id()), alpha.id(), 1)
    );
    let children = store.children(alpha.id()).expect("children");
    assert_eq!(children, vec![row.clone()]);
    assert_eq!(store.subtree(alpha.id()).expect("subtree"), vec![row]);
    assert_eq!(
        store.ancestors(child).expect("ancestors"),
        vec![run(&store, alpha.id())]
    );
    assert!(store.children(beta.id()).expect("children").is_empty());

    let root = store
        .create_run(&agent("fresh"), "fresh task")
        .expect("create_run");
    let row = run(&store, root);
    assert_eq!((row.parent, row.root, row.depth), (None, root, 0));

    // A new event in the old tree continues the global cursor after the v3 rows (1 to 10).
    let event = store
        .append(
            child,
            EventKind::AgentMessage {
                text: "after migration".into(),
            },
        )
        .expect("append");
    assert_eq!(
        store
            .events_since_global(alpha.id(), 10)
            .expect("events_since_global"),
        vec![(11, event)]
    );
}

// 2. Given a database at the current version, when it is opened again, then the migration is a
//    no-op.
#[test]
fn s2_reopening_a_v4_database_is_a_no_op() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kestrel.db");
    let message = |text: &str| EventKind::AgentMessage { text: text.into() };

    let (root, child, grandchild, runs) = {
        let store = Store::open(&path).expect("open");
        let root = store.create_run(&agent("root"), "r").expect("create_run");
        let child = store
            .create_child_run(root, &agent("child"), "c")
            .expect("create_child_run");
        let grandchild = store
            .create_child_run(child, &agent("grandchild"), "g")
            .expect("create_child_run");
        store.append(root, message("root")).expect("append");
        store.append(child, message("child")).expect("append");
        store
            .transition(grandchild, &RunStatus::Queued, RunStatus::Cancelled)
            .expect("transition");
        let runs = store.list_runs(RunFilter::default()).expect("list");
        (root, child, grandchild, runs)
    };
    let first = snapshot(&path);
    assert_eq!(first.version, SCHEMA_VERSION);

    for _ in 0..2 {
        {
            let store = Store::open(&path).expect("reopen");
            assert_eq!(store.list_runs(RunFilter::default()).expect("list"), runs);
            let subtree: Vec<RunId> = store
                .subtree(root)
                .expect("subtree")
                .iter()
                .map(|r| r.id)
                .collect();
            assert_eq!(subtree, vec![child, grandchild]);
        }
        assert_eq!(snapshot(&path), first, "reopen changed the database");
    }
}

// 2. Same for a database that reached the current version by migration: the second open changes
//    nothing.
#[test]
fn s2_reopening_a_migrated_v3_database_is_a_no_op() {
    let dir = TempDir::new().expect("tempdir");
    let path = fixture_copy(&dir);
    let runs = {
        let store = Store::open(&path).expect("open migrates");
        store.list_runs(RunFilter::default()).expect("list")
    };
    let first = snapshot(&path);
    assert_eq!(first.version, SCHEMA_VERSION);

    for _ in 0..2 {
        {
            let store = Store::open(&path).expect("reopen");
            assert_eq!(store.list_runs(RunFilter::default()).expect("list"), runs);
        }
        assert_eq!(snapshot(&path), first, "reopen changed the database");
    }
}

// 3. Given a binary and a database at a newer schema than it knows (v5 for the v4 binary of #32),
//    when it is opened, then it fails with the existing "newer schema" error.
#[test]
fn s3_newer_database_is_refused_with_the_newer_schema_error() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kestrel.db");
    {
        let store = Store::open(&path).expect("open");
        store.create_run(&agent("a"), "x").expect("create_run");
    }
    let newer = SCHEMA_VERSION + 1;
    rusqlite::Connection::open(&path)
        .expect("raw open")
        .pragma_update(None, "user_version", newer)
        .expect("bump user_version");
    let before = snapshot(&path);
    assert_eq!(before.version, newer);

    match Store::open(&path) {
        Ok(_) => panic!("opening a newer database must fail"),
        Err(e) => {
            assert!(
                e.to_string().contains("upgrade kestrel"),
                "error should tell the user to upgrade kestrel, got: {e}"
            );
            assert!(
                matches!(
                    e,
                    StoreError::SchemaTooNew { found, supported }
                        if found == newer && supported == SCHEMA_VERSION
                ),
                "expected SchemaTooNew {{ found: {newer}, supported: {SCHEMA_VERSION} }}, got {e:?}"
            );
        }
    }

    // Not migrated down and not touched.
    assert_eq!(snapshot(&path), before);
}
