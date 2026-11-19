//! Secret-looking env values are refused, whatever else changes about the placeholder rules.

#![allow(clippy::expect_used)]

mod common;

use common::{FakeRunner, spec};
use kestrel_sandbox::{GvisorDriver, SandboxDriver, SandboxError};

#[tokio::test]
async fn a_real_looking_provider_key_is_still_refused() {
    for key in [
        "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
        "sk-proj-0123456789abcdef",
    ] {
        let runner = FakeRunner::ok();
        let driver = GvisorDriver::new(runner.clone());
        let mut launch = spec();
        launch.env.insert("ANTHROPIC_API_KEY".into(), key.into());
        let err = driver.create(&launch).await.expect_err("refused");
        assert!(
            matches!(&err, SandboxError::SecretInEnv { key } if key == "ANTHROPIC_API_KEY"),
            "{err:?}"
        );
        assert!(!err.to_string().contains(key), "the value is never echoed");
        assert!(runner.all_calls().is_empty());
    }
}

#[tokio::test]
async fn the_gateway_placeholder_passes_but_only_exactly() {
    let ok = kestrel_types::GATEWAY_KEY_PLACEHOLDER;
    let near_misses = [
        format!("{ok}x"),
        format!("x{ok}"),
        format!(" {ok}"),
        format!("{ok} "),
        ok.to_uppercase(),
        ok[..ok.len() - 1].to_string(),
        format!("{ok}{ok}"),
    ];
    for key in ["ANTHROPIC_API_KEY", "openai_api_key", "GH_TOKEN"] {
        let driver = GvisorDriver::new(FakeRunner::ok());
        let mut launch = spec();
        launch.env.insert(key.into(), ok.into());
        driver.create(&launch).await.expect("exact placeholder");
        for bad in &near_misses {
            let mut launch = spec();
            launch.env.insert(key.into(), bad.clone());
            let err = driver.create(&launch).await.expect_err(bad);
            assert!(matches!(err, SandboxError::SecretInEnv { .. }), "{bad}");
        }
    }
}
