//! Scenarios 2 and 3: `kestrel run` rejects bad agent files with exit 2 and queues good ones.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Fixture, json_doc, path_str};
use kestrel_store::RunFilter;
use kestrel_types::{EventKind, MicroUsd, RunId, RunStatus};
use predicates::prelude::*;

const NEGATIVE_BUDGET: &str = r#"
name: bad
runtime: claude-code
role: tester
model_policy: { default: tier:mid }
sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: git-worktree }
budget: { usd: -1.00, wall_clock: 10m }
"#;

/// Asserts no run was written. The database may not exist at all, which is also fine.
fn assert_no_runs(f: &Fixture) {
    if f.db.exists() {
        let runs = f.store().list_runs(RunFilter::default()).expect("list");
        assert!(runs.is_empty(), "bad file still created runs: {runs:?}");
    }
}

#[test]
fn s2_bad_yaml_exits_2_naming_the_field() {
    let f = Fixture::new();
    let bad = f.write("bad.yaml", NEGATIVE_BUDGET);
    f.kestrel()
        .arg("run")
        .arg(&bad)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("usd"));
    assert_no_runs(&f);
}

#[test]
fn s2_unknown_key_exits_2_naming_it() {
    let f = Fixture::new();
    let yaml = format!("{}\nbudgett: {{ usd: 1.0 }}\n", common::GOOD_YAML);
    let bad = f.write("bad.yaml", &yaml);
    f.kestrel()
        .args(["run", path_str(&bad), "--input", "x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("budgett"));
    assert_no_runs(&f);
}

#[test]
fn s2_missing_agent_file_exits_2() {
    let f = Fixture::new();
    let missing = f.dir.path().join("nope.yaml");
    f.kestrel()
        .args(["run", path_str(&missing), "--input", "x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("nope.yaml"));
    assert_no_runs(&f);
}

#[test]
fn s2_good_file_without_input_exits_2() {
    let f = Fixture::new();
    let good = f.good_agent();
    f.kestrel()
        .arg("run")
        .arg(&good)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--input"));
    assert_no_runs(&f);
}

#[test]
fn s3_run_prints_id_and_queues_the_run() {
    let f = Fixture::new();
    let good = f.good_agent();
    let out = f
        .kestrel()
        .args(["run", path_str(&good), "--input", "x"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let id: RunId = stdout.trim().parse().expect("stdout is exactly a run id");

    let store = f.store();
    let run = store.get_run(id).expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Queued);
    assert_eq!(run.agent, "cli-tester");
    assert_eq!(run.input, "x");
    let spec = store.run_spec(id).expect("spec").expect("spec stored");
    assert_eq!(spec.budget.limit, MicroUsd(2_000_000));
}

#[test]
fn s3_run_writes_run_created_like_engine_submit() {
    let f = Fixture::new();
    let good = f.good_agent();
    let out = f
        .kestrel()
        .args(["run", path_str(&good), "--input", "x"])
        .assert()
        .success();
    let id: RunId = String::from_utf8_lossy(&out.get_output().stdout)
        .trim()
        .parse()
        .expect("run id");
    let events = f.store().events_since(id, 0).expect("events");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0].kind,
        EventKind::RunCreated {
            agent: "cli-tester".into()
        }
    );
}

#[test]
fn s3_run_json_prints_the_run_id() {
    let f = Fixture::new();
    let good = f.good_agent();
    let out = f
        .kestrel()
        .args(["run", path_str(&good), "--input", "x", "--json"])
        .assert()
        .success();
    let doc = json_doc(&out.get_output().stdout);
    let id: RunId = doc["run_id"]
        .as_str()
        .expect("run_id string")
        .parse()
        .expect("run id");
    let run = f.store().get_run(id).expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Queued);
}

#[test]
fn s3_two_runs_get_distinct_ids() {
    let f = Fixture::new();
    let good = f.good_agent();
    let mut ids = Vec::new();
    for input in ["a", "b"] {
        let out = f
            .kestrel()
            .args(["run", path_str(&good), "--input", input])
            .assert()
            .success();
        ids.push(
            String::from_utf8_lossy(&out.get_output().stdout)
                .trim()
                .to_string(),
        );
    }
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        f.store()
            .list_runs(RunFilter::default())
            .expect("list")
            .len(),
        2
    );
}
