//! Issue #43: the runtimes get the run's gateway MCP servers (Kestrel tools from #42, Travsr from
//! #44) in their config, limited to the tools the run was granted, and a short note on spawning.

mod common;

use common::{GATEWAY, MODEL, ctx, image_for};
use kestrel_agent::{
    AGENT_NOTE, API_KEY_PLACEHOLDER, CONFIG_DIR, LaunchPlan, RunContext, adapter_for,
};
use kestrel_types::{AgentSpec, Runtime};
use serde_json::{Value, json};
use url::Url;

const TASK: &str = "Fix the flaky test";

fn spec_with(runtime: Runtime, caps: &str) -> AgentSpec {
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
    match AgentSpec::from_yaml(&yaml) {
        Ok(spec) => spec,
        Err(e) => panic!("test spec must parse: {e}"),
    }
}

fn plan_for(runtime: Runtime, caps: &str, ctx: &RunContext) -> LaunchPlan {
    match adapter_for(runtime).launch_plan(&spec_with(runtime, caps), TASK, ctx) {
        Ok(plan) => plan,
        Err(e) => panic!("{runtime} plan failed: {e}"),
    }
}

const SPAWNER: &str = "{ mcp: [kestrel.spawn, kestrel.join, kestrel.ask_human, fs.read], \
    spawn: { max_children: 2, max_depth: 1, agents: [worker] } }";

/// The value after `flag` in `argv`.
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

// 1
#[test]
fn claude_code_gets_the_kestrel_server_and_only_the_granted_tools() {
    let plan = plan_for(Runtime::ClaudeCode, SPAWNER, &ctx());
    let config = format!("{CONFIG_DIR}/mcp.json");
    assert_eq!(after(&plan.argv, "--mcp-config"), Some(config.as_str()));
    let mcp = json_of(file(&plan, "mcp.json").expect("mcp.json in the plan"));
    assert_eq!(mcp["mcpServers"]["kestrel"]["type"], json!("http"));
    assert_eq!(
        mcp["mcpServers"]["kestrel"]["url"],
        json!(format!("{GATEWAY}/mcp"))
    );
    assert!(mcp["mcpServers"].get("travsr").is_none());
    assert_eq!(
        after(&plan.argv, "--allowedTools"),
        Some(
            "mcp__kestrel__kestrel_spawn,mcp__kestrel__kestrel_join,mcp__kestrel__kestrel_ask_human"
        )
    );
    assert_eq!(
        after(&plan.argv, "--append-system-prompt"),
        Some(AGENT_NOTE)
    );
    // The task still comes last, after the end of the options.
    assert_eq!(
        &plan.argv[plan.argv.len() - 2..],
        ["--".to_string(), TASK.to_string()]
    );
}

#[test]
fn spawn_without_spawn_rights_is_not_offered() {
    let plan = plan_for(
        Runtime::ClaudeCode,
        "{ mcp: [kestrel.spawn, kestrel.join] }",
        &ctx(),
    );
    assert_eq!(
        after(&plan.argv, "--allowedTools"),
        Some("mcp__kestrel__kestrel_join")
    );
}

// 2
#[test]
fn no_kestrel_or_travsr_tools_means_no_server_entry_and_no_file() {
    for runtime in [Runtime::ClaudeCode, Runtime::OpenCode] {
        let plan = plan_for(runtime, "{ mcp: [fs.read] }", &ctx());
        assert!(plan.files.is_empty(), "{runtime}: {:?}", plan.files);
        assert!(
            !plan
                .argv
                .iter()
                .any(|a| a == "--mcp-config" || a == "--allowedTools"),
            "{runtime}"
        );
        let config = plan.env.get("OPENCODE_CONFIG_CONTENT").map(|c| json_of(c));
        assert!(config.is_none_or(|c| c.get("mcp").is_none()), "{runtime}");
    }
}

// 3
#[test]
fn travsr_tools_add_a_travsr_server_and_its_tools() {
    let plan = plan_for(
        Runtime::ClaudeCode,
        "{ mcp: [travsr.search_symbol, travsr.get_callers] }",
        &ctx(),
    );
    let mcp = json_of(file(&plan, "mcp.json").expect("mcp.json in the plan"));
    assert_eq!(
        mcp["mcpServers"]["travsr"]["url"],
        json!(format!("{GATEWAY}/travsr"))
    );
    assert!(mcp["mcpServers"].get("kestrel").is_none());
    assert_eq!(
        after(&plan.argv, "--allowedTools"),
        Some("mcp__travsr__search_symbol,mcp__travsr__get_callers")
    );
    // Without Kestrel tools there is nothing to say about spawning.
    assert!(after(&plan.argv, "--append-system-prompt").is_none());
}

// 4
#[test]
fn opencode_gets_the_same_servers_as_remote_mcp_and_the_note_as_instructions() {
    let plan = plan_for(
        Runtime::OpenCode,
        "{ mcp: [kestrel.join, travsr.get_callers] }",
        &ctx(),
    );
    let config = json_of(
        plan.env
            .get("OPENCODE_CONFIG_CONTENT")
            .expect("opencode config"),
    );
    assert_eq!(
        config["mcp"]["kestrel"],
        json!({ "type": "remote", "url": format!("{GATEWAY}/mcp"), "enabled": true })
    );
    assert_eq!(
        config["mcp"]["travsr"],
        json!({ "type": "remote", "url": format!("{GATEWAY}/travsr"), "enabled": true })
    );
    // The provider setting from M1 is still there.
    assert_eq!(
        config["providers"]["anthropic"]["settings"]["baseURL"],
        json!(format!("{GATEWAY}/v1"))
    );
    assert_eq!(
        config["instructions"],
        json!([format!("{CONFIG_DIR}/kestrel.md")])
    );
    assert_eq!(file(&plan, "kestrel.md"), Some(AGENT_NOTE));
}

// 6
#[test]
fn the_env_holds_only_what_the_runtime_needs() {
    let claude = plan_for(Runtime::ClaudeCode, SPAWNER, &ctx());
    let keys: Vec<&str> = claude.env.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"
        ]
    );
    assert_eq!(claude.env["ANTHROPIC_API_KEY"], API_KEY_PLACEHOLDER);
    let open = plan_for(Runtime::OpenCode, SPAWNER, &ctx());
    let keys: Vec<&str> = open.env.keys().map(String::as_str).collect();
    assert_eq!(keys, ["ANTHROPIC_API_KEY", "OPENCODE_CONFIG_CONTENT"]);
    for plan in [&claude, &open] {
        for value in plan
            .env
            .values()
            .chain(plan.files.iter().map(|f| &f.contents))
        {
            assert!(!value.contains("sk-ant"), "{value}");
        }
    }
}

// 7
#[test]
fn server_urls_are_built_as_urls_not_strings() {
    for (gateway, want) in [
        // A token that needed escaping stays escaped, and `/mcp` is a new path segment.
        (
            "http://gw:8787/r/a%20b%2Fc",
            "http://gw:8787/r/a%20b%2Fc/mcp",
        ),
        // A run root with a trailing slash gets no double slash.
        ("http://gw:8787/r/tok/", "http://gw:8787/r/tok/mcp"),
    ] {
        let ctx = RunContext {
            gateway_url: Url::parse(gateway).unwrap_or_else(|e| panic!("{e}")),
            ..ctx()
        };
        let plan = plan_for(Runtime::ClaudeCode, SPAWNER, &ctx);
        let mcp = json_of(file(&plan, "mcp.json").expect("mcp.json"));
        assert_eq!(
            mcp["mcpServers"]["kestrel"]["url"],
            json!(want),
            "{gateway}"
        );
    }
}

// 8
#[test]
fn the_agent_note_is_short_and_holds_no_token_or_path() {
    assert!(
        AGENT_NOTE.lines().count() <= 30,
        "{} lines",
        AGENT_NOTE.lines().count()
    );
    assert!(AGENT_NOTE.contains("kestrel.spawn") && AGENT_NOTE.contains("kestrel.join"));
    for leak in [
        "tok_",
        "/r/",
        "/home",
        "/Users",
        "/workspace",
        "C:\\",
        "http",
    ] {
        assert!(!AGENT_NOTE.contains(leak), "{leak}");
    }
    // The plans carry the note as written, so they carry no token through it either.
    let plan = plan_for(Runtime::OpenCode, SPAWNER, &ctx());
    assert!(
        !file(&plan, "kestrel.md")
            .unwrap_or_default()
            .contains("tok_3f9a1c")
    );
    let _ = MODEL;
}
