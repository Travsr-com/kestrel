//! Engine requirements from the #3 and #5 reviews (comments on issue #6): the outcome comes from
//! the last `Finished` plus the sandbox exit code, output is read to the end, and every run gets a
//! fresh adapter.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::*;
use kestrel_types::{EventKind, RunStatus, Runtime};

fn has_message(h: &Harness, id: kestrel_types::RunId, text: &str) -> bool {
    h.has_event(
        id,
        |k| matches!(k, EventKind::AgentMessage { text: t } if t == text),
    )
}

#[tokio::test(start_paused = true)]
async fn last_finished_wins_when_an_error_follows_a_stop() {
    // OpenCode: `step_finish` with reason stop, then an `error` line, then exit 1.
    let mut h = Harness::new(4);
    h.driver.script(
        &["ok:step done", "msg:more output", "fail:provider error"],
        1,
    );
    h.start();
    let id = h.submit(spec()).await;

    until("failed", || h.is_failed(id).is_some()).await;
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains("provider error"), "{msg}");
    assert!(has_message(&h, id, "more output"));
}

#[tokio::test(start_paused = true)]
async fn later_ok_finished_overrides_an_earlier_failure() {
    let mut h = Harness::new(4);
    h.driver.script(&["fail:transient", "ok:recovered"], 0);
    h.start();
    let id = h.submit(spec()).await;

    until("completed", || h.is(id, &RunStatus::Completed)).await;
    assert!(h.has_event(id, |k| *k
        == EventKind::RunFinished {
            summary: Some("recovered".into())
        }));
}

#[tokio::test(start_paused = true)]
async fn output_after_finished_is_still_recorded() {
    let mut h = Harness::new(4);
    h.driver.script(&["ok:done", "msg:trailing line"], 0);
    h.start();
    let id = h.submit(spec()).await;

    until("completed", || h.is(id, &RunStatus::Completed)).await;
    assert!(has_message(&h, id, "trailing line"));
}

#[tokio::test(start_paused = true)]
async fn no_finished_and_exit_zero_completes() {
    let mut h = Harness::new(4);
    h.driver.script(&["msg:hi"], 0);
    h.start();
    let id = h.submit(spec()).await;

    until("completed", || h.is(id, &RunStatus::Completed)).await;
    until("sandbox destroyed", || h.driver.leaked().is_empty()).await;
}

#[tokio::test(start_paused = true)]
async fn no_finished_and_nonzero_exit_fails_with_the_exit_code() {
    // A crash before the result line, or an OpenCode step that ended for `length`.
    let mut h = Harness::new(4);
    h.driver.script(&["msg:hi"], 3);
    h.start();
    let id = h.submit(spec()).await;

    until("failed", || h.is_failed(id).is_some()).await;
    let msg = h.is_failed(id).unwrap_or_default();
    assert!(msg.contains('3'), "message names the exit code: {msg}");
    until("sandbox destroyed", || h.driver.leaked().is_empty()).await;
}

#[tokio::test(start_paused = true)]
async fn ok_finished_but_nonzero_exit_fails() {
    let mut h = Harness::new(4);
    h.driver.script(&["ok:looked fine"], 2);
    h.start();
    let id = h.submit(spec()).await;

    until("failed", || h.is_failed(id).is_some()).await;
}

#[tokio::test(start_paused = true)]
async fn output_is_read_to_the_end_before_the_exit_is_known() {
    // The fake's `wait` resolves only after the output stream is drained, like a real pipe.
    let mut h = Harness::new(4);
    let lines: Vec<String> = (0..500).map(|i| format!("msg:line {i}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    h.driver.script(&refs, 0);
    h.start();
    let id = h.submit(spec()).await;

    until("completed", || h.is(id, &RunStatus::Completed)).await;
    let messages = h.count_events(id, |k| matches!(k, EventKind::AgentMessage { .. }));
    assert_eq!(messages, 500);
}

#[tokio::test(start_paused = true)]
async fn every_run_gets_a_fresh_adapter() {
    let mut h = Harness::new(2);
    h.driver.script(&["msg:A-1", "msg:A-2", "ok:A-done"], 0);
    h.driver.script(&["msg:B-1", "msg:B-2", "ok:B-done"], 0);
    h.start();
    let a = h.submit(spec()).await;
    let b = h.submit(spec()).await;

    until("both completed", || {
        h.is(a, &RunStatus::Completed) && h.is(b, &RunStatus::Completed)
    })
    .await;

    assert_eq!(h.adapters.instances(), 2, "one adapter per run");
    assert_eq!(h.adapters.runtimes(), vec![Runtime::ClaudeCode; 2]);
    for plans in h.adapters.plans() {
        assert_eq!(plans.len(), 1, "each adapter launches exactly one run");
    }
    for seen in h.adapters.lines() {
        assert_eq!(seen.len(), 3, "{seen:?}");
        let tag = if seen[0].contains("A-") { "A-" } else { "B-" };
        assert!(
            seen.iter().all(|l| l.contains(tag)),
            "an adapter saw lines from more than one run: {seen:?}"
        );
    }
}
