//! An `AgentSpec` survives a JSON round trip, so the store can persist it with each run.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use kestrel_types::AgentSpec;

#[test]
fn agent_spec_round_trips_through_json() {
    let yaml = "apiVersion: kestrel/v1\nkind: Agent\nname: full\nruntime: claude-code\nrole: reviewer\n\
        model_policy: { default: tier:mid, escalate: tier:frontier }\n\
        capabilities: { mcp: [github], network: [api.github.com], secrets: [GH_TOKEN], \
        spawn: { max_children: 2, max_depth: 1, agents: [helper] } }\n\
        sandbox: { tier: apple-container, image: img, cpu: 1.5, memory: 2Gi, workspace: git-worktree }\n\
        budget: { usd: 0.25, wall_clock: 90s }\n\
        approvals: { required_for: [gh.merge] }\n\
        memory: { kind: notes }\n\
        triggers: [ { cron: \"0 * * * *\" } ]\n";
    let spec = AgentSpec::from_yaml(yaml).expect("parses");
    let json = serde_json::to_string(&spec).expect("serializes");
    let back: AgentSpec = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(back, spec);
}
