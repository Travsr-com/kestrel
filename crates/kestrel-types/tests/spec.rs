//! Scenarios 1 to 5: AgentSpec::from_yaml.

use std::time::Duration;

use kestrel_types::{AgentSpec, MicroUsd, Runtime, SandboxTier, Tier, WorkspaceSpec};

const EXAMPLE: &str = r#"
apiVersion: kestrel/v1
kind: Agent
name: pr-reviewer
runtime: claude-code
role: reviewer
model_policy: { default: tier:mid, escalate: tier:frontier }
capabilities:
  mcp: [travsr.get_blast_radius, travsr.get_callers, kestrel.spawn, kestrel.ask_human]
  network: [api.github.com]
  secrets: [github_token]
  spawn: { max_children: 4, max_depth: 2, agents: [test-writer] }
sandbox: { tier: auto, cpu: 2, memory: 4Gi, workspace: git-clone }
budget: { usd: 2.00, wall_clock: 30m }
approvals: { required_for: [gh.comment, git.push] }
memory: { scope: project }
triggers:
  - webhook: github.pull_request.opened
"#;

fn err_text(yaml: &str) -> String {
    match AgentSpec::from_yaml(yaml) {
        Ok(spec) => panic!("expected an error, got {spec:?}"),
        Err(e) => e.to_string(),
    }
}

fn with(from: &str, to: &str) -> String {
    assert!(EXAMPLE.contains(from), "fixture drift: {from}");
    EXAMPLE.replacen(from, to, 1)
}

#[test]
fn s1_parses_architecture_example() {
    let spec = AgentSpec::from_yaml(EXAMPLE).expect("example must parse");
    assert_eq!(spec.name, "pr-reviewer");
    assert_eq!(spec.runtime, Runtime::ClaudeCode);
    assert_eq!(spec.role, "reviewer");
    assert_eq!(spec.model_policy.default, Tier::Mid);
    assert_eq!(spec.model_policy.escalate, Some(Tier::Frontier));
    assert_eq!(
        spec.capabilities.mcp,
        vec![
            "travsr.get_blast_radius",
            "travsr.get_callers",
            "kestrel.spawn",
            "kestrel.ask_human"
        ]
    );
    assert_eq!(spec.capabilities.network, vec!["api.github.com"]);
    assert_eq!(spec.capabilities.secrets, vec!["github_token"]);
    let spawn = spec.capabilities.spawn.expect("spawn policy");
    assert_eq!(spawn.max_children, 4);
    assert_eq!(spawn.max_depth, 2);
    assert_eq!(spawn.agents, vec!["test-writer"]);
    assert_eq!(spec.sandbox.tier, SandboxTier::Auto);
    assert_eq!(spec.sandbox.cpu_millis, 2000);
    assert_eq!(spec.sandbox.memory_mb, 4096);
    assert_eq!(spec.sandbox.image, None);
    assert!(matches!(
        spec.sandbox.workspace,
        WorkspaceSpec::GitClone { .. }
    ));
    assert_eq!(spec.budget.limit, MicroUsd(2_000_000));
    assert_eq!(spec.budget.wall_clock, Duration::from_secs(30 * 60));
    assert_eq!(spec.approvals.required_for, vec!["gh.comment", "git.push"]);
}

#[test]
fn s2_unsupported_runtime_lists_supported_values() {
    let msg = err_text(&with("runtime: claude-code", "runtime: codex"));
    assert!(msg.contains("codex"), "{msg}");
    assert!(msg.contains("not supported"), "{msg}");
    assert!(msg.contains("claude-code"), "{msg}");
    assert!(msg.contains("opencode"), "{msg}");
}

#[test]
fn s2_opencode_runtime_is_accepted() {
    let spec = AgentSpec::from_yaml(&with("runtime: claude-code", "runtime: opencode"))
        .expect("opencode is supported");
    assert_eq!(spec.runtime, Runtime::OpenCode);
}

#[test]
fn s3_negative_budget_names_field() {
    let msg = err_text(&with("usd: 2.00", "usd: -1.00"));
    assert!(msg.contains("usd"), "{msg}");
}

#[test]
fn s3_zero_memory_names_field() {
    let msg = err_text(&with("memory: 4Gi", "memory: 0"));
    assert!(msg.contains("memory"), "{msg}");
    let msg = err_text(&with("memory: 4Gi", "memory: 0Gi"));
    assert!(msg.contains("memory"), "{msg}");
}

#[test]
fn s3_empty_name_names_field() {
    let msg = err_text(&with("name: pr-reviewer", "name: \"\""));
    assert!(msg.contains("name"), "{msg}");
}

#[test]
fn s4_sub_micro_dollar_is_rejected_not_rounded() {
    let msg = err_text(&with("usd: 2.00", "usd: 0.0000001"));
    assert!(msg.contains("usd"), "{msg}");
}

#[test]
fn s4_exactly_one_micro_dollar_is_accepted() {
    let spec = AgentSpec::from_yaml(&with("usd: 2.00", "usd: 0.000001")).expect("1 micro-dollar");
    assert_eq!(spec.budget.limit, MicroUsd(1));
}

#[test]
fn s5_unknown_top_level_key_rejected() {
    let yaml = format!("{EXAMPLE}\nbudgett: {{ usd: 1.0 }}\n");
    let msg = err_text(&yaml);
    assert!(msg.contains("budgett"), "{msg}");
}

#[test]
fn s5_garbage_yaml_is_an_error_not_a_panic() {
    assert!(AgentSpec::from_yaml("{{{ not yaml").is_err());
    assert!(AgentSpec::from_yaml("").is_err());
}
