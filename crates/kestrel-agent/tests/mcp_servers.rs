//! Issue #43: the runtimes get the run's gateway MCP servers (Kestrel tools from #42, Travsr from
//! #44) in their config, limited to the tools the run was granted, and a short note on spawning.

mod common;

use common::{GATEWAY, MODEL, ctx, image_for};
use kestrel_agent::{
    API_KEY_PLACEHOLDER, CONFIG_DIR, LaunchPlan, RunContext, adapter_for, agent_note,
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

/// The agents `SPAWNER` (and the other spawning specs here) may spawn.
fn worker() -> Vec<String> {
    vec!["worker".to_string()]
}

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
    // Only these servers: none from an `.mcp.json` the cloned repo may hold.
    assert!(plan.argv.iter().any(|a| a == "--strict-mcp-config"));
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
        agent_note(&worker(), true, true).as_deref()
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

// The gateway offers spawn only to a grant that may spawn something: depth left, and an agent.
// A child narrowed to depth 0 keeps its spawn policy, so the policy alone is not enough.
#[test]
fn spawn_is_not_offered_without_depth_left_or_an_agent_to_spawn() {
    for spawn in [
        "{ max_children: 2, max_depth: 0, agents: [worker] }",
        "{ max_children: 2, max_depth: 1, agents: [] }",
    ] {
        let caps = format!("{{ mcp: [kestrel.spawn, kestrel.join], spawn: {spawn} }}");
        let plan = plan_for(Runtime::ClaudeCode, &caps, &ctx());
        assert_eq!(
            after(&plan.argv, "--allowedTools"),
            Some("mcp__kestrel__kestrel_join"),
            "{spawn}"
        );
    }
}

// A run that cannot spawn is not told how to: a leaf child, or one that may only ask a person.
#[test]
fn the_note_covers_only_the_tools_the_run_was_offered() {
    let ask_only = agent_note(&[], true, true).expect("a note for ask_human");
    assert!(ask_only.contains("kestrel.ask_human"), "{ask_only}");
    assert!(!ask_only.contains("kestrel.spawn"), "{ask_only}");
    assert!(!ask_only.contains("kestrel.join"), "{ask_only}");
    let spawn_only = agent_note(&worker(), false, false).expect("a note for spawn");
    assert!(!spawn_only.contains("kestrel.ask_human"), "{spawn_only}");
    // A grant may hold spawn without join; then join is not described, and the only way to a
    // child's result is to wait for it.
    assert!(!spawn_only.contains("kestrel.join"), "{spawn_only}");
    assert!(spawn_only.contains("set wait"), "{spawn_only}");
    let with_join = agent_note(&worker(), true, false).expect("a note");
    assert!(!with_join.contains("set wait"), "{with_join}");
    assert!(spawn_only.contains("kestrel.spawn"), "{spawn_only}");
    assert_eq!(agent_note(&[], true, false), None);
    assert_eq!(agent_note(&[], false, false), None);
    let spawner = plan_for(
        Runtime::ClaudeCode,
        "{ mcp: [kestrel.spawn], spawn: { max_children: 1, max_depth: 1, agents: [worker] } }",
        &ctx(),
    );
    assert_eq!(
        after(&spawner.argv, "--append-system-prompt"),
        Some(spawn_only.as_str())
    );

    let leaf = plan_for(
        Runtime::ClaudeCode,
        "{ mcp: [kestrel.spawn, kestrel.join, kestrel.ask_human],          spawn: { max_children: 1, max_depth: 0, agents: [worker] } }",
        &ctx(),
    );
    assert_eq!(
        after(&leaf.argv, "--append-system-prompt"),
        Some(ask_only.as_str())
    );
    let join_only = plan_for(Runtime::ClaudeCode, "{ mcp: [kestrel.join] }", &ctx());
    assert!(after(&join_only.argv, "--append-system-prompt").is_none());
    let open = plan_for(Runtime::OpenCode, "{ mcp: [kestrel.join] }", &ctx());
    assert!(file(&open, "kestrel.md").is_none());
    let config = json_of(open.env.get("OPENCODE_CONFIG_CONTENT").expect("config"));
    assert!(config.get("instructions").is_none(), "{config}");
}

// The gateway has three Kestrel tools; a misspelt one is never offered, and none twice.
#[test]
fn unknown_or_repeated_tool_names_are_left_out() {
    let plan = plan_for(
        Runtime::ClaudeCode,
        "{ mcp: [kestrel.joinn, kestrel.join, kestrel.join, travsr.get_callers, travsr.get_callers] }",
        &ctx(),
    );
    assert_eq!(
        after(&plan.argv, "--allowedTools"),
        Some("mcp__kestrel__kestrel_join,mcp__travsr__get_callers")
    );
    let typo = plan_for(Runtime::ClaudeCode, "{ mcp: [kestrel.joinn] }", &ctx());
    assert!(typo.files.is_empty(), "{:?}", typo.files);
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
    // Still no server from the cloned repo's `.mcp.json`.
    let claude = plan_for(Runtime::ClaudeCode, "{ mcp: [fs.read] }", &ctx());
    assert!(claude.argv.iter().any(|a| a == "--strict-mcp-config"));
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
        "{ mcp: [kestrel.spawn, kestrel.join, travsr.get_callers],          spawn: { max_children: 1, max_depth: 1, agents: [worker] } }",
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
    assert_eq!(
        file(&plan, "kestrel.md"),
        agent_note(&worker(), true, false).as_deref()
    );
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
        // `/mcp` goes into the path, not after a query, as appending to the string would do.
        ("http://gw:8787/r/tok?k=v", "http://gw:8787/r/tok/mcp?k=v"),
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
    let full = agent_note(&worker(), true, true).expect("a note");
    assert!(full.lines().count() <= 30, "{} lines", full.lines().count());
    assert!(full.contains("kestrel.spawn") && full.contains("kestrel.join"));
    // What a child sees is what the engine does: a clone of the original repository, and no
    // branch handed back, so the summary is how its work returns.
    assert!(full.contains("original repository"), "{full}");
    // The model learns which agents it may name in kestrel.spawn.
    let two =
        agent_note(&["worker".to_string(), "reviewer".to_string()], true, false).expect("a note");
    assert!(two.contains("You may spawn: worker, reviewer."), "{two}");
    // Not every child gets a clone: its own spec sets its workspace.
    assert!(full.contains("agent spec"), "{full}");
    // A timed-out child is not stopped, so spawning it again would pay twice.
    assert!(full.contains("still running"), "{full}");
    assert!(full.contains("summary"), "{full}");
    assert!(!full.contains("branch"), "{full}");
    for note in [full.clone(), agent_note(&[], true, true).expect("a note")] {
        for leak in [
            "tok_",
            "/r/",
            "/home",
            "/Users",
            "/workspace",
            "C:\\",
            "http",
        ] {
            assert!(!note.contains(leak), "{leak}");
        }
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
