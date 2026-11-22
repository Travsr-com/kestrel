//! Issue #98: schema v5 makes the global event cursor an explicit `cursor INTEGER PRIMARY KEY
//! AUTOINCREMENT` column of `events`, so `VACUUM` cannot renumber it and a value is never reused.
//!
//! `tests/fixtures/v4.db` is a real v4 database with gaps in the `events` rowids;
//! `tests/fixtures/README.md` lists what it holds. Opening a database migrates it, so every test
//! works on a copy in a temp dir.

// Test helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use kestrel_store::{SCHEMA_VERSION, Store, StoreError};
use kestrel_types::{AgentSpec, Event, EventKind, RunId};
use rusqlite::types::Value;
use tempfile::TempDir;

const ALPHA: &str = "01a203e6-b979-7376-ae42-2b2c01c2ae75";
const BETA: &str = "01a203e6-b99f-7440-a2a4-ef8fc3b1efc9";
const CHILD: &str = "01a203e6-b9be-738a-a3a5-c92f7f2bbde6";

/// Every cursor in the v4 fixture, in commit order. 2, 4, 7 and 8 are gone.
const FIXTURE_CURSORS: [u64; 7] = [1, 3, 5, 6, 9, 10, 11];
/// The cursors of the tree rooted at alpha (alpha and its child).
const ALPHA_TREE_CURSORS: [u64; 5] = [1, 5, 6, 10, 11];
const BETA_CURSORS: [u64; 2] = [3, 9];

fn id(s: &str) -> RunId {
    s.parse().expect("fixture run id parses")
}

fn agent(name: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: opencode\nrole: r\nmodel_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 1.00, wall_clock: 10m }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture parses")
}

fn msg(text: &str) -> EventKind {
    EventKind::AgentMessage { text: text.into() }
}

/// Copies a checked-in fixture database into `dir` and returns the path of the copy.
fn fixture_copy(dir: &TempDir, name: &str) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let copy = dir.path().join("kestrel.db");
    std::fs::copy(&source, &copy).expect("copy the fixture");
    copy
}

/// A copy of the v4 fixture in `dir`, migrated by opening it once.
fn migrated_v4(dir: &TempDir) -> PathBuf {
    let path = fixture_copy(dir, "v4.db");
    drop(Store::open(&path).expect("open migrates"));
    path
}

/// A fresh database in `dir`.
fn fresh(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("kestrel.db");
    drop(Store::open(&path).expect("open"));
    path
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

fn exec(path: &Path, sql: &str) {
    rusqlite::Connection::open(path)
        .expect("raw open")
        .execute_batch(sql)
        .expect("execute");
}

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .expect("raw open")
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version")
}

/// `(name, declared type in upper case, part of the primary key)` per column.
fn table_columns(path: &Path, table: &str) -> Vec<(String, String, bool)> {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let mut stmt = conn
        .prepare("SELECT name, type, pk FROM pragma_table_info(?1) ORDER BY cid")
        .expect("prepare");
    stmt.query_map([table], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?.to_ascii_uppercase(),
            r.get::<_, i64>(2)? != 0,
        ))
    })
    .expect("query")
    .collect::<rusqlite::Result<Vec<_>>>()
    .expect("rows")
}

/// The `events` rows by rowid, which is what v4 handed out as the cursor. The same query works on
/// v4 and v5, because an `INTEGER PRIMARY KEY` is the rowid.
const EVENTS_BY_ROWID: &str = "SELECT rowid, run_id, seq, body FROM events ORDER BY rowid";

/// Every table other than `events`, in full, so the same queries work before and after.
const OTHER_TABLES: [&str; 6] = [
    "SELECT * FROM runs ORDER BY id",
    "SELECT * FROM run_specs ORDER BY run_id",
    "SELECT * FROM run_tokens ORDER BY run_id",
    "SELECT * FROM run_caps ORDER BY run_id",
    "SELECT * FROM model_calls ORDER BY id",
    "SELECT * FROM control ORDER BY id",
];

fn other_tables(path: &Path) -> Vec<Vec<Vec<Value>>> {
    OTHER_TABLES.iter().map(|sql| rows(path, sql)).collect()
}

/// Everything in the file: version, schema and every row of every table.
#[derive(Debug, PartialEq)]
struct Snapshot {
    version: i64,
    schema: Vec<Vec<Value>>,
    tables: Vec<(String, Vec<Vec<Value>>)>,
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
    }
}

fn cursors(stream: &[(u64, Event)]) -> Vec<u64> {
    stream.iter().map(|(cursor, _)| *cursor).collect()
}

/// Every event, read through the store in one page.
fn all_events(store: &Store) -> Vec<(u64, Event)> {
    store
        .events_after_page(0, &[], usize::MAX)
        .expect("events_after_page")
        .events
}

/// Asserts `events` is a v5 table whose first column is an explicit `cursor INTEGER PRIMARY KEY`
/// that `AUTOINCREMENT` keeps from being reused.
fn assert_explicit_cursor(path: &Path) {
    let columns = table_columns(path, "events");
    let want = [
        ("cursor", "INTEGER", true),
        ("run_id", "TEXT", false),
        ("seq", "INTEGER", false),
        ("body", "TEXT", false),
    ];
    let got: Vec<(&str, &str, bool)> = columns
        .iter()
        .map(|(name, ty, pk)| (name.as_str(), ty.as_str(), *pk))
        .collect();
    assert_eq!(got, want, "events columns in {}", path.display());

    let sql = rows(
        path,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'events'",
    );
    match sql.as_slice() {
        [row] => match row.as_slice() {
            [Value::Text(sql)] => assert!(
                sql.to_ascii_uppercase().contains("AUTOINCREMENT"),
                "events.cursor must be AUTOINCREMENT so a value is never reused: {sql}"
            ),
            other => panic!("events sql is not text: {other:?}"),
        },
        other => panic!("expected one events table, got {other:?}"),
    }

    // The tail index is still there, so a tree read after a cursor seeks.
    let indexed = rows(
        path,
        "SELECT i.name FROM sqlite_master AS m, pragma_index_info(m.name) AS i
         WHERE m.type = 'index' AND m.tbl_name = 'events' AND i.seqno = 0",
    );
    assert!(
        indexed.contains(&vec![Value::Text("run_id".into())]),
        "no index on events leads with run_id: {indexed:?}"
    );
}

// 1. Given a v4 database with gaps in the cursor, when it is migrated, then every existing event
//    keeps its cursor and no other row changes.
#[test]
fn s1_v4_database_migrates_to_v5_and_every_event_keeps_its_cursor() {
    let dir = TempDir::new().expect("tempdir");
    let path = fixture_copy(&dir, "v4.db");
    assert_eq!(user_version(&path), 4, "the fixture must be a v4 database");
    let events_before = rows(&path, EVENTS_BY_ROWID);
    let rowids: Vec<Value> = events_before.iter().map(|r| r[0].clone()).collect();
    let want: Vec<Value> = FIXTURE_CURSORS
        .iter()
        .map(|c| Value::Integer(i64::try_from(*c).expect("small")))
        .collect();
    assert_eq!(rowids, want, "the fixture must have gaps at 2, 4, 7 and 8");
    let others_before = other_tables(&path);

    let store = Store::open(&path).expect("open migrates");
    assert_eq!(SCHEMA_VERSION, 5);
    assert_eq!(user_version(&path), 5);

    assert_eq!(
        rows(
            &path,
            "SELECT cursor, run_id, seq, body FROM events ORDER BY cursor"
        ),
        events_before,
        "an event lost or changed its cursor"
    );
    assert_eq!(other_tables(&path), others_before, "migration changed rows");

    assert_eq!(cursors(&all_events(&store)), FIXTURE_CURSORS);
    let alpha = store
        .events_since_global(id(ALPHA), 0)
        .expect("events_since_global");
    assert_eq!(cursors(&alpha), ALPHA_TREE_CURSORS);
    let beta = store
        .events_since_global(id(BETA), 0)
        .expect("events_since_global");
    assert_eq!(cursors(&beta), BETA_CURSORS);
    assert_eq!(store.latest_cursor().expect("latest_cursor"), 11);

    // The events themselves read back intact through the per-run API too.
    let child: Vec<EventKind> = store
        .events_since(id(CHILD), 0)
        .expect("events_since")
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        child,
        vec![
            EventKind::RunCreated {
                agent: "child".into(),
                parent: Some(id(ALPHA)),
            },
            msg("hello from child"),
        ]
    );
}

// 1. ... then `events` has an explicit `INTEGER PRIMARY KEY` cursor: on a migrated v4 database, on
//    a v3 database migrated through v4, and on a new database.
#[test]
fn s1_events_has_an_explicit_autoincrement_cursor_column() {
    let v4 = TempDir::new().expect("tempdir");
    let v3 = TempDir::new().expect("tempdir");
    let new = TempDir::new().expect("tempdir");
    let v3_path = fixture_copy(&v3, "v3.db");
    drop(Store::open(&v3_path).expect("open migrates"));
    for path in [migrated_v4(&v4), v3_path, fresh(&new)] {
        assert_eq!(user_version(&path), SCHEMA_VERSION);
        assert_explicit_cursor(&path);
    }
}

// 1. A v3 database goes through v4 to v5 in one open and keeps its cursors 1 to 10.
#[test]
fn s1_v3_database_migrates_to_v5_and_keeps_its_cursors() {
    let dir = TempDir::new().expect("tempdir");
    let path = fixture_copy(&dir, "v3.db");
    let before = rows(&path, EVENTS_BY_ROWID);
    drop(Store::open(&path).expect("open migrates"));
    assert_eq!(
        rows(
            &path,
            "SELECT cursor, run_id, seq, body FROM events ORDER BY cursor"
        ),
        before
    );
}

// 1. The migrated table keeps its rules: one row per (run, seq), and a new event continues after
//    the newest cursor.
#[test]
fn s1_migrated_events_keep_unique_seq_and_continue_after_the_newest_cursor() {
    let dir = TempDir::new().expect("tempdir");
    let path = migrated_v4(&dir);
    let conn = rusqlite::Connection::open(&path).expect("raw open");
    assert!(
        conn.execute(
            "INSERT INTO events (run_id, seq, body) VALUES (?1, 1, '{}')",
            [BETA],
        )
        .is_err(),
        "a second seq 1 for one run must be refused"
    );
    drop(conn);

    let store = Store::open(&path).expect("reopen");
    let event = store
        .append(id(BETA), msg("after migration"))
        .expect("append");
    assert_eq!(event.seq, 3);
    assert_eq!(
        store
            .events_since_global(id(BETA), 9)
            .expect("events_since_global"),
        vec![(12, event)]
    );
}

// 1. "... so values are never reused": a cursor stays used even when its row is gone.
#[test]
fn s1_a_cursor_is_never_handed_out_twice() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kestrel.db");
    let run = {
        let store = Store::open(&path).expect("open");
        let run = store.create_run(&agent("a"), "x").expect("create_run");
        for text in ["one", "two", "three"] {
            store.append(run, msg(text)).expect("append");
        }
        run
    };
    // The newest row disappears (by hand: nothing in kestrel deletes events).
    exec(&path, "DELETE FROM events WHERE rowid = 3");

    let store = Store::open(&path).expect("reopen");
    let event = store.append(run, msg("four")).expect("append");
    assert_eq!(
        store
            .events_since_global(run, 2)
            .expect("events_since_global"),
        vec![(4, event)],
        "cursor 3 was already handed to clients and must not be reused"
    );
}

// 2. Given `VACUUM` on a database with gaps in the cursor, then the cursors are unchanged.
#[test]
fn s2_vacuum_keeps_every_cursor() {
    let dir = TempDir::new().expect("tempdir");
    let path = migrated_v4(&dir);
    let before = rows(
        &path,
        "SELECT cursor, run_id, seq, body FROM events ORDER BY cursor",
    );

    exec(&path, "VACUUM");

    assert_eq!(
        rows(
            &path,
            "SELECT cursor, run_id, seq, body FROM events ORDER BY cursor"
        ),
        before
    );
    let store = Store::open(&path).expect("open");
    assert_eq!(cursors(&all_events(&store)), FIXTURE_CURSORS);
    assert_eq!(store.latest_cursor().expect("latest_cursor"), 11);
}

// 2. ... and a client resuming with `after=<cursor>` gets no duplicate and no gap.
#[test]
fn s2_a_client_resuming_after_vacuum_gets_no_duplicate_and_no_gap() {
    let dir = TempDir::new().expect("tempdir");
    let path = migrated_v4(&dir);

    // The client reads a first page, and the tree stream up to the same point.
    let (first, tree_first, tree_after) = {
        let store = Store::open(&path).expect("open");
        let page = store.events_after_page(0, &[], 3).expect("first page");
        let tree = store
            .events_since_global_page(id(ALPHA), 0, 2)
            .expect("first tree page");
        let after = tree.last().map(|(c, _)| *c).expect("a tree page");
        (page, tree, after)
    };
    assert_eq!(cursors(&first.events), [1, 3, 5]);
    assert_eq!(first.next, 5);
    assert_eq!(tree_after, 5);

    exec(&path, "VACUUM");

    let store = Store::open(&path).expect("open after VACUUM");
    let late = store
        .append(id(CHILD), msg("after vacuum"))
        .expect("append");

    // Resume the global stream in small pages from the cursor the client holds.
    let mut seen = first.events.clone();
    let mut after = first.next;
    loop {
        let page = store.events_after_page(after, &[], 2).expect("resume page");
        if page.events.is_empty() {
            break;
        }
        after = page.next;
        seen.extend(page.events);
    }
    let got = cursors(&seen);
    let unique: HashSet<u64> = got.iter().copied().collect();
    assert_eq!(unique.len(), got.len(), "duplicate events: {got:?}");
    let mut want = FIXTURE_CURSORS.to_vec();
    want.push(12);
    assert_eq!(got, want, "gap or reorder in the resumed stream");
    assert_eq!(seen, {
        let mut all = all_events(&store);
        all.truncate(want.len());
        all
    });

    // The tree stream resumes the same way.
    let mut tree = tree_first;
    tree.extend(
        store
            .events_since_global(id(ALPHA), tree_after)
            .expect("resume tree"),
    );
    assert_eq!(cursors(&tree), [1, 5, 6, 10, 11, 12]);
    assert_eq!(tree.last().map(|(_, e)| e), Some(&late));
}

// 3. Given an older schema, then the migration is idempotent: a second and third open of a
//    migrated v4 database change nothing.
#[test]
fn s3_reopening_a_migrated_v4_database_is_a_no_op() {
    let dir = TempDir::new().expect("tempdir");
    let path = migrated_v4(&dir);
    let first = snapshot(&path);
    assert_eq!(first.version, SCHEMA_VERSION);
    for _ in 0..2 {
        let store = Store::open(&path).expect("reopen");
        assert_eq!(cursors(&all_events(&store)), FIXTURE_CURSORS);
        drop(store);
        assert_eq!(snapshot(&path), first, "reopen changed the database");
    }
}

// 3. ... and the same for a database created at v5.
#[test]
fn s3_reopening_a_new_database_is_a_no_op() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kestrel.db");
    {
        let store = Store::open(&path).expect("open");
        let run = store.create_run(&agent("a"), "x").expect("create_run");
        store.append(run, msg("one")).expect("append");
        store.append(run, msg("two")).expect("append");
    }
    let first = snapshot(&path);
    assert_eq!(first.version, SCHEMA_VERSION);
    for _ in 0..2 {
        drop(Store::open(&path).expect("reopen"));
        assert_eq!(snapshot(&path), first, "reopen changed the database");
    }
}

// 3. ... and a newer schema is refused as now, without touching the file.
#[test]
fn s3_a_newer_schema_is_refused_and_left_alone() {
    let dir = TempDir::new().expect("tempdir");
    let path = fresh(&dir);
    let newer = SCHEMA_VERSION + 1;
    rusqlite::Connection::open(&path)
        .expect("raw open")
        .pragma_update(None, "user_version", newer)
        .expect("bump user_version");
    let before = snapshot(&path);

    match Store::open(&path) {
        Ok(_) => panic!("opening a newer schema must fail"),
        Err(e) => {
            assert!(
                e.to_string().contains("upgrade kestrel"),
                "error should tell the user to upgrade kestrel, got: {e}"
            );
            match e {
                StoreError::SchemaTooNew { found, supported } => {
                    assert_eq!((found, supported), (newer, SCHEMA_VERSION));
                    assert_eq!(supported, 5);
                }
                other => panic!("expected SchemaTooNew, got {other:?}"),
            }
        }
    }
    assert_eq!(snapshot(&path), before, "a refused database was changed");
}

// During an upgrade a v4 binary that opened the file before the migration can still append: its
// insert names only `run_id, seq, body`, and the row gets the next cursor, which is also the rowid
// the v4 binary reads.
#[test]
fn an_older_binarys_insert_gets_the_next_cursor() {
    let dir = TempDir::new().expect("tempdir");
    let path = migrated_v4(&dir);
    exec(
        &path,
        &format!(
            "INSERT INTO events (run_id, seq, body)
             SELECT run_id, seq + 1, body FROM events WHERE cursor = 9 AND run_id = '{BETA}'"
        ),
    );
    assert_eq!(
        rows(
            &path,
            "SELECT cursor, rowid FROM events ORDER BY cursor DESC LIMIT 1"
        ),
        vec![vec![Value::Integer(12), Value::Integer(12)]]
    );
}
