//! Issue #95: `kestrel message <run-id> <text>` sends a message to a running agent. It is recorded
//! as an event for the agent to take with `kestrel.messages`; a run that cannot take it is refused
//! with the reason (exit 1), and a text that is not a message is a usage error (exit 2).

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{Fixture, json_doc, running_run, walk};
use kestrel_store::Store;
use kestrel_types::{AgentSpec, EventKind, RunId, RunStatus};
use predicates::prelude::*;

/// An agent that takes messages.
fn listener() -> AgentSpec {
    AgentSpec::from_yaml(
        "name: listener\nruntime: claude-code\nrole: r\n\
         capabilities: { mcp: [kestrel.messages] }\n\
         sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }\n\
         budget: { usd: 2.00, wall_clock: 10m }\n",
    )
    .expect("listener spec parses")
}

fn running_listener(store: &Store) -> RunId {
    let id = store.create_run(&listener(), "task").expect("create run");
    walk(
        store,
        id,
        RunStatus::Queued,
        &[RunStatus::Provisioning, RunStatus::Running],
    );
    id
}

fn sent(store: &Store, id: RunId) -> Vec<String> {
    store
        .events_since(id, 0)
        .expect("events")
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::MessageSent { text } => Some(text),
            _ => None,
        })
        .collect()
}

// Scenario 1, from the CLI: the message is recorded for the agent.
#[test]
fn a_message_to_a_running_run_is_recorded() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_listener(&store);
    f.kestrel()
        .args(["message", &id.to_string(), "also fix the docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("sent message to {id}")));
    assert_eq!(sent(&store, id), ["also fix the docs"]);
    // Nothing for the engine's control channel: the agent takes it from its inbox.
    assert!(store.take_control().expect("take control").is_empty());
}

#[test]
fn a_message_supports_json() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_listener(&store);
    let out = f
        .kestrel()
        .args(["message", &id.to_string(), "hi", "--json"])
        .assert()
        .success();
    let doc = json_doc(&out.get_output().stdout);
    assert_eq!(doc["run_id"], id.to_string(), "{doc}");
    assert_eq!(doc["sent"], "message", "{doc}");
    assert!(doc["seq"].as_u64().is_some(), "{doc}");
    // The text is not echoed back.
    assert!(!doc.to_string().contains("\"hi\""), "{doc}");
}

// Scenario 2: an agent that cannot take messages is refused, and the reason names the grant.
#[test]
fn an_agent_without_the_grant_is_refused_with_exit_1() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_run(&store);
    f.kestrel()
        .args(["message", &id.to_string(), "hi"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("cannot take messages"))
        .stderr(predicate::str::contains("kestrel.messages"));
    assert!(sent(&store, id).is_empty());
}

// Scenario 3: a run that is not running is refused with the reason.
#[test]
fn a_run_that_is_not_running_is_refused_with_exit_1() {
    let f = Fixture::new();
    let store = f.store();
    let queued = store.create_run(&listener(), "task").expect("create run");
    f.kestrel()
        .args(["message", &queued.to_string(), "hi"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("only a running one"));
    let done = running_listener(&store);
    walk(&store, done, RunStatus::Running, &[RunStatus::Completed]);
    f.kestrel()
        .args(["message", &done.to_string(), "hi"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("the run has ended (completed)"));
    assert!(sent(&store, queued).is_empty());
    assert!(sent(&store, done).is_empty());
}

#[test]
fn an_unknown_run_exits_1_and_a_bad_id_exits_2() {
    let f = Fixture::new();
    f.kestrel()
        .args(["message", &RunId::new().to_string(), "hi"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no such run"));
    f.kestrel().args(["message", "nope", "hi"]).assert().code(2);
    // The text is required.
    f.kestrel()
        .args(["message", &RunId::new().to_string()])
        .assert()
        .code(2);
}

#[test]
fn a_text_that_is_not_a_message_exits_2_and_writes_nothing() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_listener(&store);
    for text in [
        "   ".to_string(),
        "x".repeat(16 * 1024 + 1),
        "\u{1b}[2J".to_string(),
    ] {
        f.kestrel()
            .args(["message", &id.to_string(), &text])
            .assert()
            .code(2)
            .stderr(predicate::str::is_empty().not());
    }
    assert!(sent(&store, id).is_empty());
}

// `kestrel watch` shows the message and its delivery.
#[test]
fn watch_shows_a_message_and_its_delivery() {
    let f = Fixture::new();
    let store = f.store();
    let id = running_listener(&store);
    store.send_message(id, "also fix the docs").expect("send");
    let taken = store.take_messages(id, None).expect("take");
    assert_eq!(taken.len(), 1);
    walk(&store, id, RunStatus::Running, &[RunStatus::Completed]);
    f.kestrel()
        .args(["watch", &id.to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "message to the agent: also fix the docs",
        ))
        .stdout(predicate::str::contains("the agent read 1 message"));
}
