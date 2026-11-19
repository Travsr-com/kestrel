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
