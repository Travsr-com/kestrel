//! The real adapter launch plans must pass the real sandbox drivers' checks. Each piece is
//! tested alone elsewhere; this file runs them together (the e2e job found they disagreed: the
//! adapters' placeholder API key was refused as a secret).

#![allow(clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use kestrel_agent::{RunContext, adapter_for};
use kestrel_sandbox::{
    AppleContainerDriver, CommandOutput, CommandRunner, GvisorDriver, LaunchSpec, NetworkPolicy,
    SandboxDriver, SpawnedProcess,
};
use kestrel_types::{AgentSpec, RunId, Runtime};
use url::Url;

/// Every command succeeds and prints a docker-style container id.
struct Fake;

#[async_trait]
impl CommandRunner for Fake {
    async fn run(&self, _argv: &[String]) -> std::io::Result<CommandOutput> {
        Ok(CommandOutput {
            status: 0,
            stdout: "9f2c1d4e7a6b0c3d5e8f1a2b4c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d\n".into(),
            stderr: String::new(),
        })
    }
    async fn spawn(&self, _argv: &[String]) -> std::io::Result<SpawnedProcess> {
        Err(std::io::Error::other("no spawn"))
    }
}

fn spec(runtime: &str) -> AgentSpec {
    AgentSpec::from_yaml(&format!(
        "name: echo\nruntime: {runtime}\nrole: tester\n\
         model_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, image: my-image:1, cpu: 1, memory: 512Mi, workspace: git-clone }}\n\
         budget: {{ usd: 1, wall_clock: 10m }}\n"
    ))
    .expect("spec")
}

fn launch_spec(runtime: Runtime, name: &str) -> LaunchSpec {
    let ctx = RunContext {
        run_id: RunId::new(),
        gateway_url: Url::parse("http://172.18.0.2:7070/r/0123456789abcdef").expect("url"),
        model: "claude-haiku-4-5".into(),
    };
    let plan = adapter_for(runtime)
        .launch_plan(&spec(name), "fix the bug", &ctx)
        .expect("plan");
    LaunchSpec {
        image: plan.image,
        argv: plan.argv,
        env: plan.env,
        workspace: Some("/tmp/.tmpAbC123/data/workspaces/run-1".into()),
        cpu_millis: 1000,
        memory_mb: 512,
        network: NetworkPolicy::GatewayOnly {
            gateway: Url::parse("http://172.18.0.2:7070/").expect("url"),
        },
        labels: Default::default(),
        ..Default::default()
    }
}

#[tokio::test]
async fn adapter_launch_envs_pass_the_gvisor_secret_check() {
    let driver = GvisorDriver::new(Arc::new(Fake)).with_workspace_owner(|_| Ok((0, 0)));
    for (runtime, name) in [
        (Runtime::ClaudeCode, "claude-code"),
        (Runtime::OpenCode, "opencode"),
    ] {
        let launch = launch_spec(runtime, name);
        driver
            .create(&launch)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[tokio::test]
async fn adapter_launch_envs_pass_the_apple_container_secret_check() {
    let driver = AppleContainerDriver::new(Arc::new(Fake)).with_workspace_owner(|_| Ok((0, 0)));
    for (runtime, name) in [
        (Runtime::ClaudeCode, "claude-code"),
        (Runtime::OpenCode, "opencode"),
    ] {
        let launch = launch_spec(runtime, name);
        driver
            .create(&launch)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}
