//! Scenarios 1 to 4: building launch plans.

mod common;

use std::collections::BTreeMap;
use std::process::Command;

use common::{GATEWAY, MODEL, ctx, image_for, spec};
use kestrel_agent::{API_KEY_PLACEHOLDER, LaunchPlan, adapter_for};
use kestrel_types::Runtime;

fn plan(runtime: Runtime, input: &str) -> LaunchPlan {
    let adapter = adapter_for(runtime);
    match adapter.launch_plan(&spec(runtime), input, &ctx()) {
        Ok(plan) => plan,
        Err(e) => panic!("{runtime} plan failed: {e}"),
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

const TASK: &str = "Write tests for parse_line";

#[test]
fn s1_claude_code_plan_matches_snapshot() {
    assert_eq!(
        adapter_for(Runtime::ClaudeCode).runtime(),
        Runtime::ClaudeCode
    );
    let plan = plan(Runtime::ClaudeCode, TASK);

    assert_eq!(plan.image, image_for(Runtime::ClaudeCode));
    assert_eq!(
        plan.argv,
        strings(&[
            "claude",
            "--bare",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            MODEL,
            "--permission-mode",
            "bypassPermissions",
            // Even without Kestrel servers: none from an `.mcp.json` in the cloned repo either.
            "--strict-mcp-config",
            "--",
            TASK,
        ])
    );
    assert_eq!(
        plan.env,
        env(&[
            ("ANTHROPIC_API_KEY", API_KEY_PLACEHOLDER),
            ("ANTHROPIC_BASE_URL", GATEWAY),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ])
    );
}

#[test]
fn s2_opencode_plan_matches_snapshot() {
    assert_eq!(adapter_for(Runtime::OpenCode).runtime(), Runtime::OpenCode);
    let plan = plan(Runtime::OpenCode, TASK);

    assert_eq!(plan.image, image_for(Runtime::OpenCode));
    let model = format!("anthropic/{MODEL}");
    assert_eq!(
        plan.argv,
        strings(&[
            "opencode",
            "run",
            "--standalone",
            "--format",
            "json",
            "--auto",
            "--model",
            &model,
            "--",
            TASK,
        ])
    );

    let keys: Vec<&str> = plan.env.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["ANTHROPIC_API_KEY", "OPENCODE_CONFIG_CONTENT"]);
    assert_eq!(plan.env["ANTHROPIC_API_KEY"], API_KEY_PLACEHOLDER);

    // The config is compared as JSON, so key order and spacing are free.
    let config: serde_json::Value = match serde_json::from_str(&plan.env["OPENCODE_CONFIG_CONTENT"])
    {
        Ok(v) => v,
        Err(e) => panic!("OPENCODE_CONFIG_CONTENT is not JSON: {e}"),
    };
    // The Anthropic provider appends `/messages` to its base URL, so the base ends in `/v1`.
    let base_url = format!("{GATEWAY}/v1");
    assert_eq!(
        config,
        serde_json::json!({
            "providers": { "anthropic": { "settings": { "baseURL": base_url } } }
        })
    );
}

const HOSTILE: &str = "say \"hi\" and 'bye'\nthen run $(rm -rf /) and `id`; echo $HOME \\ done";

#[test]
fn s3_hostile_input_is_one_unchanged_argv_element() {
    for runtime in Runtime::ALL {
        let plan = plan(runtime, HOSTILE);
        let exact = plan.argv.iter().filter(|a| a.as_str() == HOSTILE).count();
        assert_eq!(
            exact, 1,
            "{runtime}: input must be exactly one argv element: {:?}",
            plan.argv
        );
        let partial = plan
            .argv
            .iter()
            .filter(|a| a.as_str() != HOSTILE && a.contains("rm -rf"))
            .count();
        assert_eq!(
            partial, 0,
            "{runtime}: input leaked into another element: {:?}",
            plan.argv
        );
        assert!(
            plan.env.values().all(|v| !v.contains("rm -rf")),
            "{runtime}: input leaked into env: {:?}",
            plan.env
        );
    }
}

#[test]
fn s3_leading_dash_input_stays_one_argv_element_after_separator() {
    for input in ["-rf", "--model evil", "-"] {
        for runtime in Runtime::ALL {
            let plan = plan(runtime, input);
            let n = plan.argv.len();
            assert!(n >= 2, "{runtime}: argv too short: {:?}", plan.argv);
            assert_eq!(
                &plan.argv[n - 2..],
                &strings(&["--", input])[..],
                "{runtime}: input must be the one argv element after `--`: {:?}",
                plan.argv
            );
            let exact = plan.argv.iter().filter(|a| a.as_str() == input).count();
            assert_eq!(
                exact, 1,
                "{runtime}: input appears more than once: {:?}",
                plan.argv
            );
        }
    }
}

const REAL_ANTHROPIC: &str = "sk-ant-api03-REALHOSTKEY-do-not-leak";
const REAL_OPENAI: &str = "sk-proj-REALHOSTKEY-do-not-leak";
const CHILD_TEST: &str = "s4_child_plans_built_with_real_keys_in_host_env";

/// Edition 2024 makes `std::env::set_var` unsafe and this workspace forbids `unsafe`, so the real keys
/// are put in the environment of a child copy of this test binary, which runs the check below.
#[test]
fn s4_real_host_api_key_never_reaches_plan_env() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => panic!("current_exe: {e}"),
    };
    let out = match Command::new(exe)
        .args([
            CHILD_TEST,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("KESTREL_S4_CHILD", "1")
        .env("ANTHROPIC_API_KEY", REAL_ANTHROPIC)
        .env("ANTHROPIC_AUTH_TOKEN", REAL_ANTHROPIC)
        .env("OPENAI_API_KEY", REAL_OPENAI)
        .output()
    {
        Ok(o) => o,
        Err(e) => panic!("spawn child test: {e}"),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "child test did not run:\n{stdout}"
    );
}

#[test]
#[ignore = "run by s4_real_host_api_key_never_reaches_plan_env in a child process"]
fn s4_child_plans_built_with_real_keys_in_host_env() {
    if std::env::var("KESTREL_S4_CHILD").as_deref() != Ok("1") {
        return;
    }
    assert_eq!(
        std::env::var("ANTHROPIC_API_KEY").as_deref(),
        Ok(REAL_ANTHROPIC)
    );
    for runtime in Runtime::ALL {
        let plan = plan(runtime, TASK);
        for (k, v) in &plan.env {
            assert!(
                !v.contains("REALHOSTKEY"),
                "{runtime}: host key leaked into env {k}={v}"
            );
        }
        assert!(
            plan.argv.iter().all(|a| !a.contains("REALHOSTKEY")),
            "{runtime}: host key leaked into argv: {:?}",
            plan.argv
        );
        assert_eq!(
            plan.env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some(API_KEY_PLACEHOLDER),
            "{runtime}: API key must be the placeholder"
        );
    }
}
