//! Input the drivers must reject before any CLI runs: flag-like images, mount-breaking
//! workspace paths, bad container ids, and secrets in env.

// Shared helpers below are not `#[test]` fns, but a panic is still the right way to fail here.
#![allow(clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{FakeRunner, ID, spec};
use kestrel_sandbox::{
    AppleContainerDriver, CommandOutput, GvisorDriver, LaunchSpec, NetworkPolicy, SandboxDriver,
    SandboxError, SandboxHandle,
};
use url::Url;

/// Both strong drivers, each with its own recording runner.
fn drivers() -> Vec<(Box<dyn SandboxDriver>, Arc<FakeRunner>)> {
    let g = FakeRunner::ok();
    let a = FakeRunner::ok();
    vec![
        (Box::new(GvisorDriver::new(g.clone())), g),
        (Box::new(AppleContainerDriver::new(a.clone())), a),
    ]
}

/// `create(spec)` fails with an error `is_expected` accepts, and nothing runs. Uses
/// `GatewayOnly` so even the network create call would show up if a check came too late.
async fn rejected(mut spec: LaunchSpec, is_expected: fn(&SandboxError) -> bool) {
    spec.network = NetworkPolicy::GatewayOnly {
        gateway: Url::parse("http://gw:1").expect("url"),
    };
    for (driver, runner) in drivers() {
        let err = driver.create(&spec).await.expect_err("must reject");
        assert!(is_expected(&err), "{}: {err:?}", driver.name());
        assert!(
            runner.all_calls().is_empty(),
            "{}: runner called: {:?}",
            driver.name(),
            runner.all_calls()
        );
    }
}

fn invalid_image(e: &SandboxError) -> bool {
    matches!(e, SandboxError::InvalidImage { .. })
}

fn invalid_workspace(e: &SandboxError) -> bool {
    matches!(e, SandboxError::InvalidWorkspace { .. })
}

fn secret(e: &SandboxError) -> bool {
    matches!(e, SandboxError::SecretInEnv { .. })
}

// A: image option injection.

#[tokio::test]
async fn flag_like_image_is_rejected() {
    for image in ["--runtime=runc", "-v", ""] {
        let mut s = spec();
        s.image = image.to_string();
        rejected(s, invalid_image).await;
    }
}

// B: mount option injection through the workspace path.

#[tokio::test]
async fn workspace_with_comma_is_rejected() {
    let mut s = spec();
    s.workspace = Some(PathBuf::from("/srv/ws,source=/"));
    rejected(s, invalid_workspace).await;
}

#[tokio::test]
async fn relative_workspace_is_rejected() {
    let mut s = spec();
    s.workspace = Some(PathBuf::from("ws/one"));
    rejected(s, invalid_workspace).await;
}

#[tokio::test]
async fn workspace_with_quote_equals_or_newline_is_rejected() {
    for path in ["/srv/\"ws", "/srv/a=b", "/srv/ws\nx", "/srv/ws\rx"] {
        let mut s = spec();
        s.workspace = Some(PathBuf::from(path));
        rejected(s, invalid_workspace).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn non_utf8_workspace_is_rejected() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let mut s = spec();
    s.workspace = Some(PathBuf::from(OsStr::from_bytes(b"/srv/\xff")));
    rejected(s, invalid_workspace).await;
}

// C: container ids.

fn printing(stdout: &'static str) -> Arc<FakeRunner> {
    FakeRunner::with(move |_| CommandOutput {
        status: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
    })
}

#[tokio::test]
async fn bad_id_from_create_is_rejected() {
    for stdout in ["", "  \n", "-rf\n", "abc def\n", "abc;rm\n"] {
        let err = GvisorDriver::new(printing(stdout))
            .create(&spec())
            .await
            .expect_err("bad id");
        assert!(
            matches!(err, SandboxError::InvalidId { .. }),
            "{stdout:?}: {err:?}"
        );
    }
}

#[tokio::test]
async fn bad_handle_id_never_reaches_the_runner() {
    let bad = SandboxHandle {
        id: "-rf".to_string(),
    };
    for (driver, runner) in drivers() {
        let start = driver.start(&bad).await.err();
        assert!(matches!(start, Some(SandboxError::InvalidId { .. })));
        assert!(matches!(
            driver.stop(&bad).await,
            Err(SandboxError::InvalidId { .. })
        ));
        assert!(matches!(
            driver.destroy(&bad).await,
            Err(SandboxError::InvalidId { .. })
        ));
        assert!(driver.wait(&bad).await.is_err());
        assert!(runner.all_calls().is_empty(), "{:?}", runner.all_calls());
    }
}

#[tokio::test]
async fn valid_id_characters_are_accepted() {
    let id = "my_box.1-a";
    let runner = printing("my_box.1-a\n");
    let h = GvisorDriver::new(runner)
        .create(&spec())
        .await
        .expect("create");
    assert_eq!(h.id, id);
    assert_ne!(h.id, ID);
}

// D: wider secret check.

fn with_env(key: &str, value: &str) -> LaunchSpec {
    let mut s = spec();
    s.env.insert(key.to_string(), value.to_string());
    s
}

#[tokio::test]
async fn lowercase_api_key_is_rejected() {
    rejected(
        with_env("anthropic_api_key", "sk-ant-api03-9xQ2mZ7vL4pR8tN1"),
        secret,
    )
    .await;
}

#[tokio::test]
async fn token_secret_password_and_aws_keys_are_rejected() {
    for key in [
        "GITHUB_TOKEN",
        "WEBHOOK_SECRET",
        "DB_PASSWORD",
        "AWS_SECRET_ACCESS_KEY",
    ] {
        rejected(
            with_env(key, "ghp_4fJx9QmZ2LwT8vRbN3kP7yHcD1sE6gUa"),
            secret,
        )
        .await;
    }
}

#[tokio::test]
async fn bracketed_real_value_is_not_a_placeholder() {
    for value in [
        "[sk-live-4fJx9QmZ2LwT8vRb]",
        "[SECRET_]",
        "[SECRET_3x]",
        "[]",
    ] {
        rejected(with_env("OPENAI_API_KEY", value), secret).await;
    }
}

#[tokio::test]
async fn exact_placeholder_passes() {
    let runner = FakeRunner::ok();
    GvisorDriver::new(runner.clone())
        .create(&with_env("GITHUB_TOKEN", "[SECRET_12]"))
        .await
        .expect("placeholder passes");
    assert_eq!(runner.runs().len(), 1);
}
