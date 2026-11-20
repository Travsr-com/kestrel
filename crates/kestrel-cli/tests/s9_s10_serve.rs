//! Scenarios 9 and 10: `kestrel serve` config checks, and a full serve / submit / SIGINT cycle.
//!
//! Config file format (TOML), as used below:
//!
//! ```toml
//! [gateway]
//! listen = "127.0.0.1:7070"        # address the LLM gateway binds; port 0 picks a free one
//!
//! [engine]
//! max_concurrent_runs = 4
//!
//! [providers.anthropic]             # each provider section is optional, but one present
//! base_url = "https://api.anthropic.com"
//! api_key = "sk-..."                # must have a non-empty api_key
//!
//! [providers.openai]
//! base_url = "https://api.openai.com"
//! api_key = "sk-..."
//!
//! [models]                          # tier -> provider model name; missing tiers use defaults
//! mid = "claude-sonnet-5-5"
//!
//! [prices."claude-sonnet-5-5"]      # USD per million tokens, exact decimals
//! input_per_mtok = "3.00"
//! output_per_mtok = "15.00"
//! cache_read_per_mtok = "0.30"
//! cache_write_per_mtok = "3.75"
//! ```
//!
//! Scenario 10 uses the hidden `--sandbox-driver fake` flag. It exists only in debug builds
//! (`cfg(debug_assertions)`), so release binaries never contain the fake driver. The fake driver
//! is named `fake` and its sandbox prints one Claude Code `result` line, then exits 0.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{ChildGuard, DEADLINE, Fixture, path_str, poll};
use kestrel_types::{EventKind, RunId, RunStatus};
use predicates::prelude::*;

const FULL_CONFIG: &str = r#"
[gateway]
listen = "127.0.0.1:0"

[engine]
max_concurrent_runs = 2

[providers.anthropic]
base_url = "http://127.0.0.1:9"
api_key = "test-anthropic-key"

[providers.openai]
base_url = "http://127.0.0.1:9"
api_key = "test-openai-key"

[models]
mid = "claude-sonnet-5-5"

[prices."claude-sonnet-5-5"]
input_per_mtok = "3.00"
output_per_mtok = "15.00"
cache_read_per_mtok = "0.30"
cache_write_per_mtok = "3.75"
"#;

/// `kestrel serve --config <path>`, run to completion, with provider env vars cleared so the
/// host environment can never fill in a missing key.
fn serve_once(f: &Fixture, config: &str) -> assert_cmd::assert::Assert {
    let path = f.write("config.toml", config);
    f.kestrel()
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .args([
            "serve",
            "--config",
            path_str(&path),
            "--sandbox-driver",
            "fake",
        ])
        .timeout(DEADLINE)
        .assert()
}

#[test]
fn s9_missing_anthropic_key_exits_2_naming_it() {
    let f = Fixture::new();
    let config = FULL_CONFIG.replace("api_key = \"test-anthropic-key\"\n", "");
    serve_once(&f, &config)
        .code(2)
        .stderr(predicate::str::contains("providers.anthropic.api_key"));
}

#[test]
fn s9_missing_openai_key_exits_2_naming_it() {
    let f = Fixture::new();
    let config = FULL_CONFIG.replace("api_key = \"test-openai-key\"\n", "");
    serve_once(&f, &config)
        .code(2)
        .stderr(predicate::str::contains("providers.openai.api_key"));
}

#[test]
fn s9_empty_key_counts_as_missing() {
    let f = Fixture::new();
    let config = FULL_CONFIG.replace("\"test-anthropic-key\"", "\"\"");
    serve_once(&f, &config)
        .code(2)
        .stderr(predicate::str::contains("providers.anthropic.api_key"));
}

#[test]
fn s9_no_provider_at_all_exits_2_naming_a_key() {
    let f = Fixture::new();
    let config = "[gateway]\nlisten = \"127.0.0.1:0\"\n";
    serve_once(&f, config)
        .code(2)
        .stderr(predicate::str::contains("api_key"));
}

#[test]
fn s9_missing_config_file_exits_2() {
    let f = Fixture::new();
    let missing = f.dir.path().join("missing.toml");
    f.kestrel()
        .args([
            "serve",
            "--config",
            path_str(&missing),
            "--sandbox-driver",
            "fake",
        ])
        .timeout(DEADLINE)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("missing.toml"));
}

#[test]
fn s9_malformed_config_exits_2() {
    let f = Fixture::new();
    serve_once(&f, "this is = not [valid toml")
        .code(2)
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn s9_bad_price_exits_2_naming_it() {
    let f = Fixture::new();
    let config = FULL_CONFIG.replace("\"15.00\"", "\"-1\"");
    serve_once(&f, &config)
        .code(2)
        .stderr(predicate::str::contains("output_per_mtok"));
}

#[test]
fn s9_unknown_sandbox_driver_exits_2() {
    let f = Fixture::new();
    let path = f.write("config.toml", FULL_CONFIG);
    f.kestrel()
        .args([
            "serve",
            "--config",
            path_str(&path),
            "--sandbox-driver",
            "nope",
        ])
        .timeout(DEADLINE)
        .assert()
        .code(2);
}

/// Starts `kestrel serve` with the fake driver as a child process.
fn start_serve(f: &Fixture) -> ChildGuard {
    let path = f.write("config.toml", FULL_CONFIG);
    let mut cmd = f.kestrel_std();
    cmd.args([
        "serve",
        "--config",
        path_str(&path),
        "--sandbox-driver",
        "fake",
    ]);
    ChildGuard::spawn(cmd)
}

/// Makes the fixture dir a git repo with one commit, so a `git-clone` agent submitted from it
/// gets its worktree from this throwaway repo instead of the repo the tests run in.
fn init_fixture_repo(f: &Fixture) {
    let dir = f.dir.path();
    if dir.join(".git").exists() {
        return;
    }
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=kestrel-test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("README.txt"), "fixture repo\n").expect("write README");
    git(&["add", "README.txt"]);
    git(&["commit", "-q", "-m", "fixture"]);
}

/// Submits a run from a separate `kestrel run` process.
fn submit(f: &Fixture) -> RunId {
    init_fixture_repo(f);
    let good = f.good_agent();
    let out = f
        .kestrel()
        .current_dir(f.dir.path())
        .args(["run", path_str(&good), "--input", "say hi"])
        .timeout(DEADLINE)
        .assert()
        .success();
    String::from_utf8_lossy(&out.get_output().stdout)
        .trim()
        .parse()
        .expect("run id")
}

fn wait_completed(f: &Fixture, serve: &mut ChildGuard, id: RunId) {
    let store = f.store();
    poll("the run to complete", || {
        if let Ok(Some(status)) = serve.child.try_wait() {
            panic!("serve exited early ({status:?}): {}", serve.stderr());
        }
        let run = store.get_run(id).expect("get").expect("run exists");
        match run.status {
            RunStatus::Completed => Some(()),
            RunStatus::Failed(e) => panic!("run failed: {e}"),
            _ => None,
        }
    });
    let events = store.events_since(id, 0).expect("events");
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            EventKind::SandboxProvisioned { driver, .. } if driver == "fake"
        )),
        "no fake sandbox: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::RunFinished { .. })),
        "no RunFinished: {events:#?}"
    );
}

fn interrupt_and_expect_clean_exit(serve: &mut ChildGuard) {
    serve.interrupt();
    let status = serve.wait_exit("serve to stop after SIGINT");
    assert!(
        status.success(),
        "serve exit {status:?}, stderr: {}",
        serve.stderr()
    );
}

#[test]
fn s10_serve_runs_a_submitted_run_and_stops_on_sigint() {
    let f = Fixture::new();
    let mut serve = start_serve(&f);
    let id = submit(&f);
    wait_completed(&f, &mut serve, id);
    interrupt_and_expect_clean_exit(&mut serve);
}

#[test]
fn s10_serve_picks_up_runs_queued_before_it_started() {
    let f = Fixture::new();
    let id = submit(&f);
    let mut serve = start_serve(&f);
    wait_completed(&f, &mut serve, id);
    interrupt_and_expect_clean_exit(&mut serve);
}

#[test]
fn s10_serve_stops_cleanly_on_sigint_when_idle() {
    let f = Fixture::new();
    let mut serve = start_serve(&f);
    // Wait until serve has opened the database, so SIGINT lands on a running server, not on
    // process startup.
    poll("serve to create the db", || {
        if let Ok(Some(status)) = serve.child.try_wait() {
            panic!("serve exited early ({status:?}): {}", serve.stderr());
        }
        f.db.exists().then_some(())
    });
    interrupt_and_expect_clean_exit(&mut serve);
}

#[test]
fn s10_serve_json_logs_are_json_lines() {
    let f = Fixture::new();
    let path = f.write("config.toml", FULL_CONFIG);
    let mut cmd = f.kestrel_std();
    cmd.args([
        "serve",
        "--config",
        path_str(&path),
        "--sandbox-driver",
        "fake",
        "--json",
    ]);
    let mut serve = ChildGuard::spawn(cmd);
    let mut lines = serve.stdout_lines();
    // The first line announces the gateway address.
    let first = common::json_line(&lines.next_line("serve ready line"));
    let addr = first["gateway"].as_str().expect("gateway address");
    assert!(addr.starts_with("127.0.0.1:"), "{first}");
    assert!(!addr.ends_with(":0"), "port 0 not resolved: {first}");
    let id = submit(&f);
    wait_completed(&f, &mut serve, id);
    interrupt_and_expect_clean_exit(&mut serve);
    for line in lines.drain() {
        common::json_line(&line);
    }
}
