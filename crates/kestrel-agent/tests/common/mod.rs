//! Shared builders for the scenario tests.

#![allow(dead_code)]

use std::path::PathBuf;

use kestrel_agent::RunContext;
use kestrel_types::{AgentSpec, RunId, Runtime};
use url::Url;

pub const GATEWAY: &str = "http://kestrel-gateway.internal:8787/r/tok_3f9a1c";
pub const MODEL: &str = "claude-sonnet-5";

pub fn image_for(runtime: Runtime) -> String {
    format!("ghcr.io/travsr-com/kestrel-{}:test", runtime.as_str())
}

pub fn spec(runtime: Runtime) -> AgentSpec {
    let yaml = format!(
        r#"
name: test-writer
runtime: {runtime}
role: tester
model_policy: {{ default: tier:mid }}
sandbox: {{ tier: auto, image: "{image}", cpu: 1, memory: 2Gi, workspace: git-clone }}
budget: {{ usd: 1.00, wall_clock: 10m }}
"#,
        runtime = runtime.as_str(),
        image = image_for(runtime),
    );
    match AgentSpec::from_yaml(&yaml) {
        Ok(spec) => spec,
        Err(e) => panic!("test spec must parse: {e}"),
    }
}

pub fn ctx() -> RunContext {
    let gateway_url = match Url::parse(GATEWAY) {
        Ok(u) => u,
        Err(e) => panic!("test gateway url must parse: {e}"),
    };
    RunContext {
        run_id: RunId::new(),
        gateway_url,
        model: MODEL.to_string(),
    }
}

pub fn fixture(rel: &str) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel);
    match std::fs::read_to_string(&path) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect(),
        Err(e) => panic!("read fixture {}: {e}", path.display()),
    }
}
