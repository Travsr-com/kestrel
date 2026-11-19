//! Scenario 6: `kestrel ps --json` parses as JSON and lists the runs in a fixture db.
//!
//! Each item: `id`, `agent`, `status` (serde form of `RunStatus`), `spent` (integer micro-USD),
//! `created_at` (RFC 3339). Items come in creation order.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;

use common::{Fixture, agent, json_doc, running_run, walk};
use kestrel_types::{MicroUsd, ModelCallRecord, RunId, RunStatus};
use predicates::prelude::*;

struct Runs {
    queued: RunId,
    running: RunId,
    done: RunId,
}

fn fixture_runs(f: &Fixture) -> Runs {
    let store = f.store();
    let queued = store.create_run(&agent(), "a").expect("create");
    let running = running_run(&store);
    store
        .record_model_call(&ModelCallRecord {
            run_id: running,
            provider: "anthropic".into(),
            model: "claude-sonnet-5-5".into(),
            tokens_in: 10,
            tokens_out: 20,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            cost: Some(MicroUsd(1234)),
            latency_ms: 5,
            status: 200,
        })
        .expect("record");
    let done = running_run(&store);
    walk(&store, done, RunStatus::Running, &[RunStatus::Completed]);
    Runs {
        queued,
        running,
        done,
    }
}

fn ids(doc: &serde_json::Value) -> Vec<String> {
    doc.as_array()
        .expect("ps --json is an array")
        .iter()
        .map(|r| r["id"].as_str().expect("id string").to_string())
        .collect()
}

#[test]
fn s6_ps_json_lists_active_runs() {
    let f = Fixture::new();
    let r = fixture_runs(&f);
    let out = f.kestrel().args(["ps", "--json"]).assert().success();
    let doc = json_doc(&out.get_output().stdout);
    assert_eq!(ids(&doc), vec![r.queued.to_string(), r.running.to_string()]);

    let running = &doc[1];
    assert_eq!(running["agent"], "cli-tester");
    assert_eq!(running["status"], "Running");
    assert_eq!(running["spent"], 1234);
    let created = running["created_at"].as_str().expect("created_at string");
    assert!(created.contains('T'), "not RFC 3339: {created}");
    assert_eq!(doc[0]["status"], "Queued");
    assert_eq!(doc[0]["spent"], 0);
}

#[test]
fn s6_ps_all_json_lists_finished_runs_too() {
    let f = Fixture::new();
    let r = fixture_runs(&f);
    let out = f
        .kestrel()
        .args(["ps", "--all", "--json"])
        .assert()
        .success();
    let doc = json_doc(&out.get_output().stdout);
    let got: BTreeSet<String> = ids(&doc).into_iter().collect();
    let want: BTreeSet<String> = [r.queued, r.running, r.done]
        .iter()
        .map(RunId::to_string)
        .collect();
    assert_eq!(got, want);
    assert_eq!(doc[2]["status"], "Completed");
}

#[test]
fn s6_ps_json_on_an_empty_db_is_an_empty_array() {
    let f = Fixture::new();
    let out = f.kestrel().args(["ps", "--json"]).assert().success();
    assert_eq!(json_doc(&out.get_output().stdout), serde_json::json!([]));
}

#[test]
fn s6_ps_text_shows_id_agent_and_status() {
    let f = Fixture::new();
    let r = fixture_runs(&f);
    f.kestrel()
        .arg("ps")
        .assert()
        .success()
        .stdout(predicate::str::contains(r.running.to_string()))
        .stdout(predicate::str::contains("cli-tester"))
        .stdout(predicate::str::contains("Running"))
        .stdout(predicate::str::contains(r.done.to_string()).not());
}

#[test]
fn s6_global_flags_work_after_the_subcommand() {
    let f = Fixture::new();
    let r = fixture_runs(&f);
    let out = assert_cmd::Command::cargo_bin("kestrel")
        .expect("kestrel binary")
        .args(["ps", "--json", "--db"])
        .arg(&f.db)
        .assert()
        .success();
    assert_eq!(
        ids(&json_doc(&out.get_output().stdout)),
        vec![r.queued.to_string(), r.running.to_string()]
    );
}

/// Without `--db` the database lives under the user data dir (`$XDG_DATA_HOME/kestrel` on Linux).
#[cfg(target_os = "linux")]
#[test]
fn s6_default_db_is_under_the_data_dir() {
    let f = Fixture::new();
    let data = f.dir.path().join("data");
    let home = f.dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    assert_cmd::Command::cargo_bin("kestrel")
        .expect("kestrel binary")
        .env("XDG_DATA_HOME", &data)
        .env("HOME", &home)
        .args(["ps", "--json"])
        .assert()
        .success();
    assert!(data.join("kestrel").join("kestrel.db").exists());
}
