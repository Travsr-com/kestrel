//! Issue #95: messages a person sends to a running agent. A message is stored as a
//! `message_sent` event, only for a running run whose agent takes messages, and handed to the
//! agent once (`kestrel.messages`), which a `message_delivered` event records.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use kestrel_store::{InboxMessage, Store, StoreError};
use kestrel_types::{AgentSpec, EventKind, MAX_MESSAGE_BYTES, RunId, RunStatus, SuspendReason};

fn spec(mcp: &str) -> AgentSpec {
    AgentSpec::from_yaml(&format!(
        "name: a\nruntime: claude-code\nrole: r\ncapabilities: {{ mcp: [{mcp}] }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
         budget: {{ usd: \"1\", wall_clock: 1h }}\n"
    ))
    .unwrap()
}

fn listener() -> AgentSpec {
    spec("kestrel.join, kestrel.messages")
}

/// A run of `agent` moved to `Running`.
fn running(store: &Store, agent: &AgentSpec) -> RunId {
    let id = store.create_run(agent, "x").unwrap();
    store
        .transition(id, &RunStatus::Queued, RunStatus::Provisioning)
        .unwrap();
    store
        .transition(id, &RunStatus::Provisioning, RunStatus::Running)
        .unwrap();
    id
}

fn kinds(store: &Store, id: RunId) -> Vec<EventKind> {
    store
        .events_since(id, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

fn message_events(store: &Store, id: RunId) -> Vec<EventKind> {
    kinds(store, id)
        .into_iter()
        .filter(|k| {
            matches!(
                k,
                EventKind::MessageSent { .. } | EventKind::MessageDelivered { .. }
            )
        })
        .collect()
}

fn refusal(e: StoreError) -> String {
    match e {
        StoreError::MessageRefused(why) => why,
        other => panic!("expected a refused message, got {other}"),
    }
}

// Scenario 1: a running run whose agent takes messages stores it as an event.
#[test]
fn a_message_to_a_running_run_is_recorded() {
    let store = Store::open_in_memory().unwrap();
    let id = running(&store, &listener());
    let sent = store.send_message(id, "also fix the docs").unwrap();
    assert_eq!(sent.run_id, id);
    assert_eq!(
        sent.kind,
        EventKind::MessageSent {
            text: "also fix the docs".into()
        }
    );
    assert_eq!(kinds(&store, id).last(), Some(&sent.kind));
    assert!(store.takes_messages(id).unwrap());
}

// Scenario 2: an agent that does not take messages refuses them, whatever its status.
#[test]
fn an_agent_without_kestrel_messages_refuses_them() {
    let store = Store::open_in_memory().unwrap();
    let id = running(&store, &spec("kestrel.join"));
    let why = refusal(store.send_message(id, "hi").unwrap_err());
    assert!(why.contains("cannot take messages"), "{why}");
    assert!(why.contains("kestrel.messages"), "{why}");
    assert!(message_events(&store, id).is_empty());
    assert!(!store.takes_messages(id).unwrap());
    let row = store.get_run(id).unwrap().unwrap();
    assert_eq!(store.message_refusal(&row).unwrap(), Some(why));
}

// Scenario 3: a run that is not running refuses a message, with the reason.
#[test]
fn a_run_that_is_not_running_refuses_a_message_with_the_reason() {
    let store = Store::open_in_memory().unwrap();
    let queued = store.create_run(&listener(), "x").unwrap();
    let why = refusal(store.send_message(queued, "hi").unwrap_err());
    assert!(why.contains("a queued run cannot take a message"), "{why}");

    let paused = running(&store, &listener());
    store
        .transition(
            paused,
            &RunStatus::Running,
            RunStatus::Suspended(SuspendReason::Manual),
        )
        .unwrap();
    let why = refusal(store.send_message(paused, "hi").unwrap_err());
    assert!(why.contains("only a running one"), "{why}");

    let done = running(&store, &listener());
    store
        .transition(done, &RunStatus::Running, RunStatus::Completed)
        .unwrap();
    let why = refusal(store.send_message(done, "hi").unwrap_err());
    assert!(why.contains("the run has ended (completed)"), "{why}");

    for id in [queued, paused, done] {
        assert!(message_events(&store, id).is_empty());
        // Whether its agent takes messages does not depend on its status.
        assert!(store.takes_messages(id).unwrap());
    }
    let row = store.get_run(done).unwrap().unwrap();
    assert!(
        store
            .message_refusal(&row)
            .unwrap()
            .unwrap()
            .contains("ended")
    );
    let live = store
        .get_run(running(&store, &listener()))
        .unwrap()
        .unwrap();
    assert_eq!(store.message_refusal(&live).unwrap(), None);
}

#[test]
fn an_ended_run_says_it_ended_before_what_its_agent_takes() {
    let store = Store::open_in_memory().unwrap();
    let id = running(&store, &spec("kestrel.join"));
    store
        .transition(id, &RunStatus::Running, RunStatus::Cancelled)
        .unwrap();
    let why = refusal(store.send_message(id, "hi").unwrap_err());
    assert!(why.contains("the run has ended (cancelled)"), "{why}");
}

#[test]
fn a_message_that_is_empty_too_long_or_not_text_is_refused() {
    let store = Store::open_in_memory().unwrap();
    let id = running(&store, &listener());
    for bad in [
        String::new(),
        "  \n".to_string(),
        "x".repeat(MAX_MESSAGE_BYTES + 1),
        "\u{1b}[2J".to_string(),
    ] {
        assert!(matches!(
            store.send_message(id, &bad),
            Err(StoreError::MessageRefused(_))
        ));
    }
    assert!(message_events(&store, id).is_empty());
    store
        .send_message(id, &"x".repeat(MAX_MESSAGE_BYTES))
        .unwrap();
}

#[test]
fn a_message_to_an_unknown_run_is_not_found() {
    let store = Store::open_in_memory().unwrap();
    let ghost = RunId::new();
    assert!(matches!(
        store.send_message(ghost, "hi"),
        Err(StoreError::NotFound(id)) if id == ghost
    ));
    assert!(!store.takes_messages(ghost).unwrap());
}

// Delivered once, oldest first, and the delivery is an event naming what it delivered.
#[test]
fn messages_are_taken_once_in_order_and_their_delivery_is_recorded() {
    let store = Store::open_in_memory().unwrap();
    let id = running(&store, &listener());
    assert_eq!(store.take_messages(id).unwrap(), []);
    assert!(
        message_events(&store, id).is_empty(),
        "nothing to deliver writes nothing"
    );

    let first = store.send_message(id, "first").unwrap();
    let second = store.send_message(id, "second").unwrap();
    let taken = store.take_messages(id).unwrap();
    assert_eq!(
        taken,
        [
            InboxMessage {
                seq: first.seq,
                text: "first".into()
            },
            InboxMessage {
                seq: second.seq,
                text: "second".into()
            },
        ]
    );
    assert_eq!(store.take_messages(id).unwrap(), []);

    let third = store.send_message(id, "third").unwrap();
    assert_eq!(
        store.take_messages(id).unwrap(),
        [InboxMessage {
            seq: third.seq,
            text: "third".into()
        }]
    );
    assert_eq!(
        message_events(&store, id),
        [
            EventKind::MessageSent {
                text: "first".into()
            },
            EventKind::MessageSent {
                text: "second".into()
            },
            EventKind::MessageDelivered {
                seqs: vec![first.seq, second.seq]
            },
            EventKind::MessageSent {
                text: "third".into()
            },
            EventKind::MessageDelivered {
                seqs: vec![third.seq]
            },
        ]
    );
}

#[test]
fn messages_of_one_run_never_reach_another() {
    let store = Store::open_in_memory().unwrap();
    let a = running(&store, &listener());
    let b = running(&store, &listener());
    store.send_message(a, "for a").unwrap();
    assert_eq!(store.take_messages(b).unwrap(), []);
    assert_eq!(store.take_messages(a).unwrap().len(), 1);
}

// Two takers at once (two calls of the tool, or two engines on one file) never get the same
// message twice, and lose none.
#[test]
fn takers_racing_on_one_file_deliver_each_message_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kestrel.db");
    let store = Arc::new(Store::open(&path).unwrap());
    let id = running(&store, &listener());
    const N: usize = 40;
    let senders = std::thread::spawn({
        let store = store.clone();
        move || {
            for i in 0..N {
                store.send_message(id, &format!("m{i}")).unwrap();
            }
        }
    });
    let takers: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                let mut got = Vec::new();
                for _ in 0..200 {
                    got.extend(store.take_messages(id).unwrap());
                }
                got
            })
        })
        .collect();
    senders.join().unwrap();
    let mut got: Vec<InboxMessage> = takers.into_iter().flat_map(|t| t.join().unwrap()).collect();
    got.extend(store.take_messages(id).unwrap());
    let mut texts: Vec<String> = got.into_iter().map(|m| m.text).collect();
    texts.sort();
    let mut want: Vec<String> = (0..N).map(|i| format!("m{i}")).collect();
    want.sort();
    assert_eq!(texts, want);
}
