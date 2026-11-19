//! Schema v2: every run keeps its full agent spec, and v1 databases migrate forward.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_store::{SCHEMA_VERSION, Store};
use kestrel_types::{AgentSpec, RunId, RunStatus};
use tempfile::TempDir;

fn agent() -> AgentSpec {
    let yaml = "name: keeper\nruntime: opencode\nrole: r\nmodel_policy: { default: tier:cheap, escalate: tier:frontier }\n\
        capabilities: { mcp: [github], network: [api.github.com] }\n\
        sandbox: { tier: gvisor, image: img, cpu: 500m, memory: 1Gi, workspace: git-worktree }\n\
        budget: { usd: 2.50, wall_clock: 30m }\n\
        approvals: { required_for: [gh.comment] }\n";
    AgentSpec::from_yaml(yaml).expect("fixture parses")
}

/// The v1 schema exactly as the first release wrote it.
const V1: &str = "
CREATE TABLE runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, input TEXT NOT NULL,
    status TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE events (run_id TEXT NOT NULL, seq INTEGER NOT NULL, body TEXT NOT NULL,
    UNIQUE (run_id, seq));
CREATE TABLE model_calls (id INTEGER PRIMARY KEY, run_id TEXT NOT NULL, cost INTEGER,
    body TEXT NOT NULL);
CREATE INDEX model_calls_run ON model_calls (run_id);
CREATE TABLE control (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL,
    cmd TEXT NOT NULL, claimed INTEGER NOT NULL DEFAULT 0);
PRAGMA user_version = 1;
";

fn version(path: &std::path::Path) -> i64 {
    rusqlite::Connection::open(path)
        .expect("raw open")
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version")
}

#[test]
fn fresh_store_keeps_the_spec_of_each_run() {
    let store = Store::open_in_memory().expect("open");
    let spec = agent();
    let id = store.create_run(&spec, "task").expect("create_run");
    assert_eq!(store.run_spec(id).expect("run_spec"), Some(spec));
    assert_eq!(store.run_spec(RunId::new()).expect("run_spec"), None);
}

#[test]
fn spec_survives_reopening_the_file() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("k.db");
    let id = {
        let store = Store::open(&path).expect("open");
        store.create_run(&agent(), "task").expect("create_run")
    };
    let store = Store::open(&path).expect("reopen");
    assert_eq!(store.run_spec(id).expect("run_spec"), Some(agent()));
    assert_eq!(version(&path), SCHEMA_VERSION);
}

#[test]
fn v1_database_migrates_forward_and_keeps_its_runs() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("v1.db");
    let old = RunId::new();
    {
        let conn = rusqlite::Connection::open(&path).expect("raw open");
        conn.execute_batch(V1).expect("v1 schema");
        conn.execute(
            "INSERT INTO runs VALUES (?1, 'old', 'in', '\"Queued\"', '\"2026-01-01T00:00:00Z\"')",
            [old.to_string()],
        )
        .expect("v1 run");
    }
    assert_eq!(version(&path), 1);

    let store = Store::open(&path).expect("open migrates");
    assert_eq!(version(&path), SCHEMA_VERSION);
    let row = store.get_run(old).expect("get_run").expect("old run kept");
    assert_eq!((row.agent.as_str(), row.status), ("old", RunStatus::Queued));
    // A run from before v2 has no stored spec.
    assert_eq!(store.run_spec(old).expect("run_spec"), None);

    let new = store.create_run(&agent(), "task").expect("create_run");
    assert_eq!(store.run_spec(new).expect("run_spec"), Some(agent()));
}

#[test]
fn corrupt_spec_is_an_error_not_a_panic() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("k.db");
    let store = Store::open(&path).expect("open");
    let id = store.create_run(&agent(), "task").expect("create_run");
    rusqlite::Connection::open(&path)
        .expect("raw open")
        .execute("UPDATE run_specs SET spec = '{'", [])
        .expect("corrupt");
    assert!(store.run_spec(id).is_err());
}
