//! Issue #89: `docker rm --force` answering that removal of the container is already in progress.
//! The driver waits until the container is gone, polling `docker inspect`, and only then reports
//! success. Any other failure stays an error, as before.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{FakeRunner, ID, s};
use kestrel_sandbox::{
    AppleContainerDriver, CommandOutput, DockerRuncDriver, GvisorDriver, SandboxDriver,
    SandboxError, SandboxHandle,
};

fn handle() -> SandboxHandle {
    SandboxHandle { id: ID.to_string() }
}

/// Docker's exact answer when another `rm` of the same container is still running.
fn in_progress() -> CommandOutput {
    fail(&format!(
        "Error response from daemon: removal of container {ID} is already in progress\n"
    ))
}

fn gone() -> CommandOutput {
    fail(&format!("Error: No such container: {ID}\n"))
}

fn fail(stderr: &str) -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

fn still_there() -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: "removing\n".to_string(),
        stderr: String::new(),
    }
}

fn rm() -> Vec<String> {
    s(&["docker", "rm", "--force", ID])
}

fn inspect() -> Vec<String> {
    s(&[
        "docker",
        "inspect",
        "--type",
        "container",
        "--format",
        "{{.State.Status}}",
        ID,
    ])
}

/// `rm` answers "already in progress"; `inspect` answers `still_there` `polls` times, then `last`.
fn removing(polls: usize, last: fn() -> CommandOutput) -> Arc<FakeRunner> {
    let inspected = Arc::new(AtomicUsize::new(0));
    FakeRunner::with(move |argv| {
        if argv.get(1).map(String::as_str) == Some("rm") {
            return in_progress();
        }
        if inspected.fetch_add(1, Ordering::SeqCst) < polls {
            still_there()
        } else {
            last()
        }
    })
}

// Scenario 1: a removal already in progress is waited for, and then destroy succeeds.

#[tokio::test(start_paused = true)]
async fn s1_gvisor_waits_until_a_container_being_removed_is_gone() {
    let runner = removing(2, gone);

    GvisorDriver::new(runner.clone())
        .destroy(&handle())
        .await
        .expect("destroy succeeds once the container is gone");

    assert_eq!(runner.runs(), vec![rm(), inspect(), inspect(), inspect()]);
}

#[tokio::test(start_paused = true)]
async fn s1_docker_runc_waits_until_a_container_being_removed_is_gone() {
    let runner = removing(1, gone);

    DockerRuncDriver::new(runner.clone())
        .destroy(&handle())
        .await
        .expect("destroy succeeds once the container is gone");

    assert_eq!(runner.runs(), vec![rm(), inspect(), inspect()]);
}

#[tokio::test(start_paused = true)]
async fn s1_a_container_already_gone_at_the_first_look_is_done_at_once() {
    let runner = removing(0, gone);

    GvisorDriver::new(runner.clone())
        .destroy(&handle())
        .await
        .expect("destroy succeeds");

    assert_eq!(runner.runs(), vec![rm(), inspect()]);
}

#[tokio::test(start_paused = true)]
async fn s1_destroy_after_a_removal_in_progress_is_still_idempotent() {
    let runner = removing(0, gone);
    let driver = GvisorDriver::new(runner.clone());

    driver.destroy(&handle()).await.expect("first destroy");
    driver.destroy(&handle()).await.expect("second destroy");

    assert_eq!(runner.runs(), vec![rm(), inspect(), rm(), inspect()]);
}

#[tokio::test(start_paused = true)]
async fn s1_a_removal_that_never_finishes_is_an_error_naming_the_container() {
    let runner = removing(usize::MAX, gone);
    let started = tokio::time::Instant::now();

    let err = GvisorDriver::new(runner.clone())
        .destroy(&handle())
        .await
        .expect_err("a container that stays is not destroyed");

    let text = err.to_string();
    assert!(text.contains(ID), "{text}");
    assert!(text.contains("still being removed"), "{text}");
    let runs = runner.runs();
    assert_eq!(runs[0], rm());
    assert!(runs.len() > 2, "polled more than once: {runs:?}");
    assert!(runs[1..].iter().all(|r| *r == inspect()), "{runs:?}");
    // Bounded: the wait ends, in virtual time, well within a minute.
    assert!(started.elapsed() <= std::time::Duration::from_secs(60));
}

// Scenario 2: a destroy that fails for another reason is still an error.

#[tokio::test(start_paused = true)]
async fn s2_a_failed_inspect_while_waiting_is_an_error_with_docker_s_text() {
    let runner = removing(0, || fail("Cannot connect to the Docker daemon\n"));

    let err = GvisorDriver::new(runner.clone())
        .destroy(&handle())
        .await
        .expect_err("inspect failed");

    match &err {
        SandboxError::CommandFailed { stderr, .. } => {
            assert!(
                stderr.contains("Cannot connect to the Docker daemon"),
                "{err}"
            );
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    assert_eq!(runner.runs(), vec![rm(), inspect()]);
}

#[tokio::test]
async fn s2_a_destroy_that_fails_for_another_reason_is_an_error_and_never_waits() {
    let runner = FakeRunner::with(|_| fail("Error response from daemon: could not kill: busy\n"));

    let err = GvisorDriver::new(runner.clone())
        .destroy(&handle())
        .await
        .expect_err("rm failed");

    match &err {
        SandboxError::CommandFailed { code, stderr, .. } => {
            assert_eq!(*code, 1);
            assert!(stderr.contains("could not kill: busy"), "{err}");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
    assert_eq!(runner.runs(), vec![rm()], "no inspect");
}

#[tokio::test]
async fn s2_apple_container_has_no_removal_in_progress_answer_so_it_stays_an_error() {
    let runner = FakeRunner::with(|_| in_progress());

    let err = AppleContainerDriver::new(runner.clone())
        .destroy(&handle())
        .await
        .expect_err("not a docker driver");

    assert!(matches!(err, SandboxError::CommandFailed { .. }), "{err:?}");
    assert_eq!(
        runner.runs(),
        vec![s(&["container", "delete", "--force", ID])]
    );
}
