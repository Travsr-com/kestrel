//! M1 end-to-end scenarios (issue #8): the real `kestrel` binary, a real gVisor sandbox running the
//! echo agent image, and the gateway in front of a mock Anthropic upstream with known usage.
//!
//! Needs: Docker with the `runsc` runtime, the image `kestrel-echo-agent:test` (built from
//! `tests/e2e/echo-agent`), git, and `KESTREL_BIN` pointing at a built `kestrel` binary.
//! Run with `cargo test -p kestrel-e2e --features e2e --test e2e -- --test-threads=1`.
//! The helpers are in `tests/common/mod.rs`, shared with the M2 suites.

// Helpers panic on purpose: a panic is how a test fails.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use common::{
    Env, FAKE_KEY, MODEL, ONE_CALL_MICRO_USD, README_START, RUN_DEADLINE, USAGE_IN, USAGE_OUT,
    agent_yaml, assert_success, git, parse_events, seqs, status_event, status_name, ts,
};

// Scenarios ---------------------------------------------------------------------------------

/// Scenario 1: a run completes in gVisor, edits its workspace, and costs exactly the mock's usage
/// times the configured prices.
#[test]
fn s1_run_completes_edits_workspace_and_reports_exact_cost() {
    let env = Env::new();
    let _serve = env.serve();
    let id = env.submit("5", "edit the readme");

    let row = env.wait_status(&id, "Completed");
    assert_eq!(row["agent"], "echo");
    assert_eq!(
        row["spent"].as_i64(),
        Some(ONE_CALL_MICRO_USD),
        "spent should be exactly one mock call at the configured prices: {row}"
    );

    let readme = std::fs::read_to_string(env.workspace(&id).join("README.txt")).expect("README");
    assert_eq!(
        readme,
        format!("{README_START}echo-agent: edit the readme\n")
    );

    // Issue #24: the sandbox runs as the workspace owner. The image's user is uid 1000 and the CI
    // runner is not, so a file the agent CREATES (env.txt, unlike README.txt, did not exist before)
    // must belong to the host user, and the host must be able to delete it.
    {
        use std::os::unix::fs::MetadataExt;
        let workspace = env.workspace(&id);
        let host_uid = std::fs::metadata(&workspace).expect("workspace dir").uid();
        let created = workspace.join("env.txt");
        let agent_uid = std::fs::metadata(&created)
            .expect("env.txt, created by the agent")
            .uid();
        if host_uid != 0 {
            assert_eq!(
                agent_uid, host_uid,
                "files the agent creates belong to the host user, not to the image's user or root"
            );
        }
        std::fs::remove_file(&created).expect("the host can delete a file the agent created");
    }

    // Model traffic went through the gateway: the mock saw exactly one call, with the real key.
    assert_eq!(env.mock.keys_seen(), vec![FAKE_KEY.to_string()]);

    let events = env.events(&id);
    let calls: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"]["kind"] == "model_call")
        .collect();
    assert_eq!(calls.len(), 1, "one model_call event expected: {events:?}");
    let call = &calls[0]["kind"];
    assert_eq!(call["model"], MODEL);
    assert_eq!(call["tokens_in"].as_u64(), Some(USAGE_IN));
    assert_eq!(call["tokens_out"].as_u64(), Some(USAGE_OUT));
    assert_eq!(call["cost"].as_i64(), Some(ONE_CALL_MICRO_USD));

    // Numbers for docs/milestones/m1.md, printed to the CI log.
    if let (Some(prov), Some(running)) = (
        status_event(&events, "Provisioning"),
        status_event(&events, "Running"),
    ) {
        let cold = ts(running) - ts(prov);
        eprintln!("m1-result: sandbox cold start (Provisioning to Running) = {cold}");
    }
    eprintln!(
        "m1-result: gateway model call latency_ms = {} (mock upstream on loopback)",
        call["latency_ms"]
    );
}

/// Scenario 2: a run over its budget suspends, survives a SIGKILL of `kestrel serve`, and
/// completes after a restart, a budget raise and a resume.
#[test]
fn s2_budget_suspend_survives_restart_and_resumes() {
    let env = Env::new();
    let serve = env.serve();
    // One call costs 6465 micro-USD, so a 0.001 USD budget is exceeded by the first call. The
    // agent sleeps after that call, so the engine suspends it while it is still running.
    let id = env.submit("0.001", "over budget sleep=60");

    let row = env.wait_status(&id, "Suspended(Budget)");
    assert!(
        row["spent"].as_i64() >= Some(ONE_CALL_MICRO_USD),
        "spend should include the first call: {row}"
    );

    serve.kill();
    let _serve = env.serve();
    assert_eq!(status_name(&env.row(&id)["status"]), "Suspended(Budget)");

    env.ok(&["budget", &id, "1"]);
    env.ok(&["resume", &id]);
    let row = env.wait_status(&id, "Completed");
    assert!(
        row["spent"].as_i64() >= Some(ONE_CALL_MICRO_USD),
        "spend survives the restart: {row}"
    );
    let readme = std::fs::read_to_string(env.workspace(&id).join("README.txt")).expect("README");
    assert!(
        readme.ends_with("echo-agent: over budget sleep=60\n"),
        "the resumed agent finished its edit: {readme:?}"
    );
}

/// Scenario 3: from inside the sandbox, hosts other than the gateway are unreachable.
#[test]
fn s3_sandbox_cannot_reach_other_hosts() {
    let env = Env::new();
    let _serve = env.serve();
    let id = env.submit("5", "check egress");
    env.wait_status(&id, "Completed");

    let egress = std::fs::read_to_string(env.workspace(&id).join("egress.txt")).expect("egress");
    let lines: Vec<&str> = egress.lines().collect();
    assert_eq!(lines.len(), 2, "two hosts tried: {egress:?}");
    for line in lines {
        // "blocked <curl exit code> <url>". Only a DNS failure (6), a refused connection (7) or
        // a timeout (28) proves the network is closed. A TLS or certificate code (35, 60) would
        // mean the host answered, so the block would be vacuous.
        let mut parts = line.split(' ');
        assert_eq!(parts.next(), Some("blocked"), "egress should fail: {line}");
        let code = parts.next().and_then(|c| c.parse::<i32>().ok());
        assert!(
            matches!(code, Some(6 | 7 | 28)),
            "egress failed for the wrong reason: {line}"
        );
    }
    // The same sandbox did reach the gateway, so the block is selective.
    assert_eq!(env.mock.keys_seen().len(), 1);
}

/// Scenario 4: the sandbox env has the gateway URL and a placeholder key, never the real key.
#[test]
fn s4_sandbox_env_has_no_provider_key() {
    let env = Env::new();
    let _serve = env.serve();
    let id = env.submit("5", "print env");
    env.wait_status(&id, "Completed");

    let vars = std::fs::read_to_string(env.workspace(&id).join("env.txt")).expect("env.txt");
    assert!(
        !vars.contains(FAKE_KEY),
        "provider key leaked into the sandbox:\n{vars}"
    );
    assert!(
        vars.lines()
            .any(|l| l.starts_with("ANTHROPIC_BASE_URL=http")),
        "the agent is pointed at the gateway:\n{vars}"
    );
    let key = vars
        .lines()
        .find_map(|l| l.strip_prefix("ANTHROPIC_API_KEY="))
        .unwrap_or_else(|| panic!("ANTHROPIC_API_KEY missing:\n{vars}"));
    assert!(
        !key.starts_with("sk-ant-"),
        "key should be a placeholder, got {key}"
    );
}

/// Scenario 5: `kestrel watch` started mid-run prints every event from seq 1 and follows the run
/// to the end, then exits on its own.
#[test]
fn s5_watch_mid_run_replays_from_start_and_follows() {
    let env = Env::new();
    let _serve = env.serve();
    let id = env.submit("5", "watch me sleep=8");
    env.wait_status(&id, "Running");

    let watch = env
        .cmd(&["watch", &id, "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start kestrel watch");
    let started = Instant::now();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(watch.wait_with_output());
    });
    let out = rx
        .recv_timeout(RUN_DEADLINE)
        .unwrap_or_else(|_| panic!("kestrel watch did not exit within {RUN_DEADLINE:?}"))
        .expect("kestrel watch output");
    assert_success(&["watch", &id, "--json"], &out);
    let events = parse_events(&String::from_utf8(out.stdout).expect("utf-8"));

    let seqs = seqs(&events);
    let want: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, want, "every event from seq 1, in order, no gaps");
    assert_eq!(events[0]["kind"]["kind"], "run_created");
    assert!(
        status_event(&events, "Running").is_some(),
        "events from before watch started are replayed"
    );
    let last_status = events
        .iter()
        .rev()
        .find(|e| e["kind"]["kind"] == "status_changed")
        .expect("a status change");
    assert_eq!(status_name(&last_status["kind"]["to"]), "Completed");
    assert!(
        events.iter().any(|e| e["kind"]["kind"] == "agent_message"),
        "the agent's message, written after watch started, is followed: {events:?}"
    );
    // The run was still sleeping when watch started, so watch had to follow it for a while.
    assert!(
        started.elapsed() > Duration::from_secs(1),
        "watch returned too early"
    );
    assert_eq!(
        env.events(&id).len(),
        events.len(),
        "watch saw every event the run has"
    );
}

/// Scenario 6 (issue #22): the agent runs `git status`, `git add` and `git commit` inside its
/// gVisor sandbox. The commit lands on `kestrel/run-<id tail>` in the run's clone only; the source
/// repo gets nothing until `fetch_back` brings the branch home.
#[test]
fn s6_agent_can_commit_in_its_clone() {
    let env = Env::new();
    let _serve = env.serve();
    let id = env.submit("5", "commit your work git=1");
    env.wait_status(&id, "Completed");

    let tail = &id[id.len().saturating_sub(12)..];
    let branch = format!("kestrel/run-{tail}");
    let workspace = env.workspace(&id);
    let report = std::fs::read_to_string(workspace.join("git.txt"))
        .expect("git.txt (written by the agent after its commit)");
    let lines: Vec<&str> = report.lines().collect();
    assert!(
        lines.contains(&format!("branch={branch}").as_str()),
        "the agent was not on its run branch:\n{report}"
    );
    for ok in ["status_rc=0", "add_rc=0", "commit_rc=0"] {
        assert!(lines.contains(&ok), "{ok} missing:\n{report}");
    }
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("log=") && l.ends_with("agent commit")),
        "no `agent commit` in git log:\n{report}"
    );

    // The source repo is untouched: still only `main`, still only the initial commit.
    let source_branches = git(&env.repo, &["branch", "--format=%(refname:short)"]);
    assert_eq!(source_branches.trim(), "main");
    assert_eq!(
        git(&env.repo, &["rev-list", "--all", "--count"]).trim(),
        "1"
    );
    assert!(!env.repo.join("agent-commit.txt").exists());

    // fetch_back (from the source repo, hooks off) brings the commit home as the run branch.
    let run: kestrel_types::RunId = id.parse().expect("run id");
    let workspaces = kestrel_core::Workspaces::new(
        env.dir.path().join("workspaces"),
        std::sync::Arc::new(kestrel_sandbox::ProcessRunner),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(workspaces.fetch_back(run, &env.repo))
        .expect("fetch_back");
    assert_eq!(
        git(&env.repo, &["show", &format!("{branch}:agent-commit.txt")]).trim(),
        "agent work"
    );
    assert_eq!(
        git(
            &env.repo,
            &["branch", "--format=%(refname:short)", "--list", &branch]
        )
        .trim(),
        branch
    );
    assert_eq!(
        git(&env.repo, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main",
        "fetch_back must not move the user's checkout"
    );
}

// Issue #42, scenario 13: an agent in a gVisor sandbox reaches the Kestrel tools over MCP, through
// the gateway bridge, at its run's /r/<token>/mcp, and sees the tools its capability allows.
#[test]
fn s7_sandbox_reaches_the_kestrel_tools_over_mcp() {
    let env = Env::new();
    let _serve = env.serve();
    let id = env.submit("5", "list the tools mcp=1");
    env.wait_status(&id, "Completed");
    let text = std::fs::read_to_string(env.workspace(&id).join("mcp.txt"))
        .unwrap_or_else(|e| panic!("no mcp.txt ({e}): {}", env.dump_events(&id)));
    assert!(
        text.contains("\"protocolVersion\":\"2025-06-18\""),
        "{text}"
    );
    assert!(text.contains("\"name\":\"kestrel\""), "{text}");
    assert!(text.contains("kestrel.join"), "{text}");
    assert!(text.contains("kestrel.ask_human"), "{text}");
    // Spawn is not in the capability, so it is not offered.
    assert!(!text.contains("kestrel.spawn"), "{text}");
    // A tool call comes back as an event stream through the bridge: no children to wait for.
    assert!(text.contains("event: message"), "{text}");
    assert!(text.contains("\"children\":[]"), "{text}");
}

// Issue #43, scenario 9: the runtime finds the Kestrel server in the MCP config mounted read-only
// at /kestrel, and a kestrel.spawn it makes there reaches the gateway under gVisor and starts a
// child run that completes.
#[test]
fn s8_agent_spawns_a_child_through_its_mcp_config() {
    let env = Env::new();
    // `kestrel serve` looks spawned agents up by name next to its database.
    let agents = env.dir.path().join("agents");
    std::fs::create_dir(&agents).expect("mkdir agents");
    std::fs::write(
        agents.join("echo.yaml"),
        agent_yaml("echo", "1", "mcp: [kestrel.join]"),
    )
    .expect("write child agent");
    let lead = env.dir.path().join("lead.yaml");
    std::fs::write(
        &lead,
        agent_yaml(
            "lead",
            "5",
            "mcp: [kestrel.spawn, kestrel.join], spawn: { max_children: 2, max_depth: 1, agents: [echo] }",
        ),
    )
    .expect("write lead agent");
    let _serve = env.serve();
    let id = env.submit_file(&lead, "delegate spawn=1");
    env.wait_status(&id, "Completed");
    let text = std::fs::read_to_string(env.workspace(&id).join("spawn.txt"))
        .unwrap_or_else(|e| panic!("no spawn.txt ({e}): {}", env.dump_events(&id)));
    assert!(
        text.contains("allowed=mcp__kestrel__kestrel_spawn,mcp__kestrel__kestrel_join"),
        "{text}"
    );
    assert!(text.contains("config_writable=no"), "{text}");
    assert!(text.contains("url=ok"), "{text}");
    assert!(text.contains(r#""name":"kestrel.spawn""#), "{text}");

    let events = env.events(&id);
    let spawned = events
        .iter()
        .find(|e| e["kind"]["kind"] == "child_spawned")
        .unwrap_or_else(|| panic!("no child_spawned event: {}", env.dump_events(&id)));
    assert_eq!(spawned["kind"]["agent"], "echo");
    let child = spawned["kind"]["child"]
        .as_str()
        .expect("child run id")
        .to_string();
    // The call waited for the child, so its id is in the answer the agent got.
    assert!(text.contains(&child), "{text}");
    env.wait_status(&child, "Completed");
}
