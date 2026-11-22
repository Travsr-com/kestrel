//! Issue #32, review follow-up: a v4 database stays readable while an older (v3) binary still
//! writes to it, and a tree's events can be tailed without scanning all of them.
//!
//! During an upgrade, `kestrel run` from the old binary can insert a run after `kestrel serve`
//! from the new one migrated the file. The old binary's insert names only the v3 columns, so the
//! run has no `root_run_id` unless the database fills it in.

// Test helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use kestrel_store::{RunFilter, Store};
use kestrel_types::{AgentSpec, EventKind, RunId};
use tempfile::TempDir;

fn agent(name: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: opencode\nrole: r\nmodel_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 1.00, wall_clock: 10m }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture parses")
}

/// A fresh v4 database file in `dir`.
fn fresh(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("kestrel.db");
    drop(Store::open(&path).expect("open"));
    path
}

/// A copy of the checked-in v3 fixture in `dir`, migrated to v4.
fn migrated_fixture(dir: &TempDir) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3.db");
    let path = dir.path().join("kestrel.db");
    std::fs::copy(&source, &path).expect("copy the v3 fixture");
    drop(Store::open(&path).expect("open migrates"));
    path
}

/// Inserts a run exactly as the v3 `create_run` did: only the five v3 columns.
fn insert_like_v3(path: &Path) -> RunId {
    let id = RunId::new();
    let conn = rusqlite::Connection::open(path).expect("raw open");
    conn.execute(
        "INSERT INTO runs (id, agent, input, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            id.to_string(),
            "old-binary",
            "task",
            "\"Queued\"",
            "\"2026-11-20T10:00:00Z\""
        ],
    )
    .expect("v3 insert");
    id
}

/// A run written by a v3 binary into a v4 file reads back as a root, and the tree calls work.
fn v3_writer_run_is_a_root(path: &Path) {
    let old = insert_like_v3(path);
    let store = Store::open(path).expect("reopen");

    let listed = store.list_runs(RunFilter::default()).expect("list_runs");
    let row = listed
        .iter()
        .find(|r| r.id == old)
        .expect("the v3 run is listed");
    assert_eq!(row.parent, None);
    assert_eq!(row.root, old);
    assert_eq!(row.depth, 0);
    assert_eq!(store.get_run(old).expect("get_run"), Some(row.clone()));
    assert_eq!(store.ancestors(old).expect("ancestors"), Vec::new());

    let child = store
        .create_child_run(old, &agent("kid"), "sub task")
        .expect("child of a v3-written run");
    let kid = store
        .get_run(child)
        .expect("get_run")
        .expect("child exists");
    assert_eq!((kid.parent, kid.root, kid.depth), (Some(old), old, 1));

    let event = store
        .append(child, EventKind::AgentMessage { text: "hi".into() })
        .expect("append");
    let stream = store.events_since_global(old, 0).expect("global read");
    assert_eq!(stream.len(), 1);
    assert_eq!(stream[0].1, event);
}

#[test]
fn a_run_inserted_by_a_v3_writer_into_a_new_v4_file_becomes_its_own_root() {
    let dir = TempDir::new().expect("tempdir");
    v3_writer_run_is_a_root(&fresh(&dir));
}

#[test]
fn a_run_inserted_by_a_v3_writer_into_a_migrated_file_becomes_its_own_root() {
    let dir = TempDir::new().expect("tempdir");
    v3_writer_run_is_a_root(&migrated_fixture(&dir));
}

#[test]
fn a_new_child_keeps_the_root_it_was_given() {
    // The fill-in must only touch rows that arrive without a root.
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(fresh(&dir)).expect("open");
    let top = store.create_run(&agent("top"), "t").expect("create_run");
    let mid = store
        .create_child_run(top, &agent("mid"), "m")
        .expect("child");
    let low = store
        .create_child_run(mid, &agent("low"), "l")
        .expect("grandchild");
    let row = store.get_run(low).expect("get_run").expect("exists");
    assert_eq!((row.parent, row.root, row.depth), (Some(mid), top, 2));
}

/// Column lists of every index on `table`, in key order.
fn index_columns(path: &Path, table: &str) -> Vec<Vec<String>> {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_index_list(?1)")
        .expect("prepare");
    let names = stmt
        .query_map([table], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("index names");
    names
        .iter()
        .map(|name| {
            let mut cols = conn
                .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
                .expect("prepare");
            cols.query_map([name], |r| r.get::<_, String>(0))
                .expect("query")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("index columns")
        })
        .collect()
}

#[test]
fn events_have_an_index_on_run_id_alone_so_a_tail_read_seeks_by_cursor() {
    // An index on `run_id` alone is keyed `(run_id, cursor)` (`events.cursor` is the rowid), so
    // `cursor > ?` is a range seek per run instead of a scan of every event the run ever wrote.
    let run_id_only = vec!["run_id".to_string()];
    for path in [
        fresh(&TempDir::new().expect("tempdir")),
        migrated_fixture(&TempDir::new().expect("tempdir")),
    ] {
        let found = index_columns(&path, "events");
        assert!(
            found.contains(&run_id_only),
            "no index on events(run_id) in {}: {found:?}",
            path.display()
        );
    }
}
