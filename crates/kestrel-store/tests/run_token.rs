//! Schema v3: every run gets a random gateway token, and budget changes are stored with the spec.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;

use kestrel_store::{SCHEMA_VERSION, Store, StoreError};
use kestrel_types::{AgentSpec, MicroUsd, RunId};
use tempfile::TempDir;

fn agent() -> AgentSpec {
    let yaml = "name: a\nruntime: opencode\nrole: r\nmodel_policy: { default: tier:cheap }\n\
        sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }\n\
        budget: { usd: 1.00, wall_clock: 10m }\n";
    AgentSpec::from_yaml(yaml).expect("fixture parses")
}

#[test]
fn each_run_gets_a_unique_random_token_that_resolves_back() {
    let store = Store::open_in_memory().expect("open");
    let mut seen = HashSet::new();
    for _ in 0..50 {
        let id = store.create_run(&agent(), "x").expect("create_run");
        let token = store.run_token(id).expect("run_token").expect("has token");
        assert_eq!(token.len(), 32, "128 bits as hex: {token}");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
        assert_ne!(token, id.to_string());
        assert!(!token.contains(&id.to_string().replace('-', "")));
        assert_eq!(store.run_by_token(&token).expect("run_by_token"), Some(id));
        assert!(seen.insert(token), "token repeated");
    }
}

#[test]
fn unknown_token_and_unknown_run_resolve_to_none() {
    let store = Store::open_in_memory().expect("open");
    let id = store.create_run(&agent(), "x").expect("create_run");
    assert_eq!(store.run_by_token("nope").expect("run_by_token"), None);
    assert_eq!(
        store.run_by_token(&id.to_string()).expect("run_by_token"),
        None
    );
    assert_eq!(store.run_token(RunId::new()).expect("run_token"), None);
}

#[test]
fn token_and_budget_survive_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("k.db");
    let (id, token) = {
        let store = Store::open(&path).expect("open");
        let id = store.create_run(&agent(), "x").expect("create_run");
        store
            .set_budget(id, MicroUsd(7_000_000))
            .expect("set_budget");
        (id, store.run_token(id).expect("run_token").expect("token"))
    };
    let store = Store::open(&path).expect("reopen");
    assert_eq!(store.run_token(id).expect("run_token"), Some(token.clone()));
    assert_eq!(store.run_by_token(&token).expect("run_by_token"), Some(id));
    let spec = store.run_spec(id).expect("run_spec").expect("spec");
    assert_eq!(spec.budget.limit, MicroUsd(7_000_000));
    // Only the budget changed.
    let mut expected = agent();
    expected.budget.limit = MicroUsd(7_000_000);
    assert_eq!(spec, expected);
    let version: i64 = rusqlite::Connection::open(&path)
        .expect("raw open")
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version");
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn set_budget_on_a_run_without_a_spec_is_not_found() {
    let store = Store::open_in_memory().expect("open");
    let ghost = RunId::new();
    match store.set_budget(ghost, MicroUsd(1)) {
        Err(StoreError::NotFound(r)) => assert_eq!(r, ghost),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn v2_database_migrates_to_v3_and_old_runs_have_no_token() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("v2.db");
    let id = {
        let store = Store::open(&path).expect("open");
        store.create_run(&agent(), "x").expect("create_run")
    };
    {
        // Turn the file back into a v2 database.
        let conn = rusqlite::Connection::open(&path).expect("raw open");
        conn.execute_batch("DROP TABLE run_tokens; PRAGMA user_version = 2;")
            .expect("downgrade");
    }
    let store = Store::open(&path).expect("open migrates");
    assert_eq!(store.run_token(id).expect("run_token"), None);
    assert!(store.run_spec(id).expect("run_spec").is_some());
    let new = store.create_run(&agent(), "y").expect("create_run");
    assert!(store.run_token(new).expect("run_token").is_some());
}
