//! Issue #95: a run granted `kestrel.messages` gets the Kestrel server with that tool, in both
//! runtimes, and a note telling it to check for messages from a person between steps.

mod common;

use common::{GATEWAY, ctx, image_for};
use kestrel_agent::{CONFIG_DIR, LaunchPlan, adapter_for, agent_note};
use kestrel_types::{AgentSpec, Runtime};
use serde_json::{Value, json};

const TASK: &str = "Fix the flaky test";

fn plan_for(runtime: Runtime, caps: &str) -> LaunchPlan {
    let yaml = format!(
        r#"
name: lead
runtime: {runtime}
role: lead
model_policy: {{ default: tier:mid }}
capabilities: {caps}
sandbox: {{ tier: auto, image: "{image}", cpu: 1, memory: 2Gi, workspace: git-clone }}
budget: {{ usd: 1.00, wall_clock: 10m }}
"#,
        runtime = runtime.as_str(),
        image = image_for(runtime),
    );
    let spec = match AgentSpec::from_yaml(&yaml) {
        Ok(spec) => spec,
        Err(e) => panic!("test spec must parse: {e}"),
    };
    match adapter_for(runtime).launch_plan(&spec, TASK, &ctx()) {
        Ok(plan) => plan,
        Err(e) => panic!("{runtime} plan failed: {e}"),
    }
}

fn after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .map(String::as_str)
}

fn file<'a>(plan: &'a LaunchPlan, name: &str) -> Option<&'a str> {
    plan.files
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.contents.as_str())
}

fn json_of(text: &str) -> Value {
    match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => panic!("not JSON ({e}): {text}"),
    }
}

/// What the note must say about messages, whatever else it says.
fn tells_to_check(note: &str) {
    assert!(note.contains("kestrel.messages"), "{note}");
    assert!(note.contains("between steps"), "{note}");
    assert!(note.contains("person"), "{note}");
    // Review of #95: the agent passes the last seq it got as `after`, so a retry loses nothing.
    assert!(note.contains("after"), "{note}");
    assert!(note.contains("seq"), "{note}");
}

#[test]
fn claude_code_gets_the_messages_tool_and_a_note() {
    let plan = plan_for(Runtime::ClaudeCode, "{ mcp: [kestrel.messages] }");
    let mcp = json_of(file(&plan, "mcp.json").expect("mcp.json in the plan"));
    assert_eq!(
        mcp["mcpServers"]["kestrel"]["url"],
        json!(format!("{GATEWAY}/mcp"))
    );
    assert_eq!(
        after(&plan.argv, "--mcp-config"),
        Some(format!("{CONFIG_DIR}/mcp.json").as_str())
    );
    assert_eq!(
        after(&plan.argv, "--allowedTools"),
        Some("mcp__kestrel__kestrel_messages")
    );
    let note = after(&plan.argv, "--append-system-prompt").expect("a note");
    tells_to_check(note);
    // Only messages were offered: no word on spawning or asking.
    assert!(!note.contains("kestrel.spawn"), "{note}");
    assert!(!note.contains("kestrel.ask_human"), "{note}");
    assert_eq!(
        &plan.argv[plan.argv.len() - 2..],
        ["--".to_string(), TASK.to_string()]
    );
}

#[test]
fn opencode_gets_the_messages_tool_and_a_note() {
    let plan = plan_for(Runtime::OpenCode, "{ mcp: [kestrel.messages] }");
    let config = json_of(&plan.env["OPENCODE_CONFIG_CONTENT"]);
    assert_eq!(
        config["mcp"]["kestrel"]["url"],
        json!(format!("{GATEWAY}/mcp"))
    );
    assert_eq!(
        config["instructions"],
        json!([format!("{CONFIG_DIR}/kestrel.md")])
    );
    tells_to_check(file(&plan, "kestrel.md").expect("the note file"));
}

#[test]
fn the_messages_note_comes_after_the_other_tools() {
    let caps = "{ mcp: [kestrel.messages, kestrel.spawn, kestrel.join, kestrel.ask_human], \
                spawn: { max_children: 2, max_depth: 1, agents: [worker] } }";
    let plan = plan_for(Runtime::ClaudeCode, caps);
    assert_eq!(
        after(&plan.argv, "--allowedTools"),
        Some(
            "mcp__kestrel__kestrel_messages,mcp__kestrel__kestrel_spawn,\
             mcp__kestrel__kestrel_join,mcp__kestrel__kestrel_ask_human"
        )
    );
    let note = after(&plan.argv, "--append-system-prompt").expect("a note");
    let others = agent_note(&["worker".to_string()], true, true).expect("the other tools' note");
    assert!(note.starts_with(&others), "{note}");
    tells_to_check(&note[others.len()..]);
}

#[test]
fn a_run_without_the_grant_is_not_told_about_messages() {
    let plan = plan_for(Runtime::ClaudeCode, "{ mcp: [kestrel.ask_human] }");
    let note = after(&plan.argv, "--append-system-prompt").expect("a note");
    assert!(!note.contains("kestrel.messages"), "{note}");
    let none = plan_for(Runtime::ClaudeCode, "{ mcp: [] }");
    assert_eq!(after(&none.argv, "--append-system-prompt"), None);
    assert_eq!(after(&none.argv, "--mcp-config"), None);
}
