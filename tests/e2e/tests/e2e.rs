//! M1 end-to-end scenarios (issue #8): the real `kestrel` binary, a real gVisor sandbox running the
//! echo agent image, and the gateway in front of a mock Anthropic upstream with known usage.
//!
//! Needs: Docker with the `runsc` runtime, the image `kestrel-echo-agent:test` (built from
//! `tests/e2e/echo-agent`), git, and `KESTREL_BIN` pointing at a built `kestrel` binary.
//! Run with `cargo test -p kestrel-e2e --features e2e -- --test-threads=1`.

// Helpers panic on purpose: a panic is how a test fails.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The provider key in the config. Fake, so it is safe to search for it in the sandbox env.
const FAKE_KEY: &str = "sk-ant-e2e-fake-key-3f9a1c7d";
/// The model every tier maps to, priced in the config below.
const MODEL: &str = "claude-e2e-1";
/// What the mock reports for every call.
const USAGE_IN: u64 = 1000;
const USAGE_OUT: u64 = 200;
const USAGE_CACHE_READ: u64 = 300;
const USAGE_CACHE_WRITE: u64 = 100;
/// Prices in USD per million tokens, as written in the config.
const PRICE_IN: &str = "3";
const PRICE_OUT: &str = "15";
const PRICE_CACHE_READ: &str = "0.30";
const PRICE_CACHE_WRITE: &str = "3.75";
/// One call in micro-USD: 1000 * 3 + 200 * 15 + 300 * 0.30 + 100 * 3.75 = 3000 + 3000 + 90 + 375.
const ONE_CALL_MICRO_USD: i64 = 6465;
/// README.txt as committed in the test repo. The echo agent appends one line to it.
const README_START: &str = "kestrel e2e\n";

const POLL: Duration = Duration::from_millis(250);
/// Covers a gVisor cold start on a CI runner with room to spare.
const RUN_DEADLINE: Duration = Duration::from_secs(180);
const SERVE_START: Duration = Duration::from_secs(60);

fn kestrel_bin() -> PathBuf {
    match std::env::var_os("KESTREL_BIN") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => panic!(
            "KESTREL_BIN is not set. Build the CLI first and point at it: \
             cargo build -p kestrel-cli && KESTREL_BIN=$PWD/target/debug/kestrel \
             cargo test -p kestrel-e2e --features e2e -- --test-threads=1"
        ),
    }
}

// Mock Anthropic upstream ------------------------------------------------------------------

/// The `x-api-key` of every request the mock received.
type Seen = Arc<Mutex<Vec<String>>>;

struct Mock {
    addr: SocketAddr,
    seen: Seen,
}

impl Mock {
    /// Starts the mock on its own thread and runtime. It lives until the test process exits.
    fn start() -> Mock {
        let seen = Seen::default();
        let state = seen.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("mock runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind mock");
                tx.send(listener.local_addr().expect("mock addr"))
                    .expect("send mock addr");
                let app = axum::Router::new()
                    .route("/v1/messages", post(messages))
                    .with_state(state);
                axum::serve(listener, app).await.expect("serve mock");
            });
        });
        let addr = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("mock start");
        Mock { addr, seen }
    }

    fn keys_seen(&self) -> Vec<String> {
        self.seen.lock().expect("mock lock").clone()
    }
}

/// A fixed, non-streaming Messages API response that echoes the requested model.
async fn messages(
    State(seen): State<Seen>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    seen.lock().expect("mock lock").push(key);
    Json(json!({
        "id": "msg_e2e_mock",
        "type": "message",
        "role": "assistant",
        "model": body["model"].as_str().unwrap_or(MODEL),
        "content": [{"type": "text", "text": "hello from the mock"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": USAGE_IN,
            "output_tokens": USAGE_OUT,
            "cache_read_input_tokens": USAGE_CACHE_READ,
            "cache_creation_input_tokens": USAGE_CACHE_WRITE
        }
    }))
}

// Test environment ------------------------------------------------------------------------

/// One isolated Kestrel setup: data dirs, database, config, a git repo and the mock.
struct Env {
    dir: TempDir,
    repo: PathBuf,
    db: PathBuf,
    config: PathBuf,
    mock: Mock,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).expect("mkdir repo");
        std::fs::write(repo.join("README.txt"), README_START).expect("write README");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["add", "README.txt"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=e2e",
                "-c",
                "user.email=e2e@example.invalid",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        let mock = Mock::start();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, config_toml(mock.addr)).expect("write config");
        // Private, like a real config: serve warns about a config other users can read.
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("chmod config");
        let db = dir.path().join("kestrel.db");
        Env {
            dir,
            repo,
            db,
            config,
            mock,
        }
    }

    /// A command for the kestrel binary, isolated in this env's dirs and run from the repo, so
    /// `workspace: git-worktree` (repo `.`) resolves to the test repo.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(kestrel_bin());
        cmd.args(args)
            .arg("--db")
            .arg(&self.db)
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .current_dir(&self.repo);
        cmd
    }

    /// Runs a kestrel command to the end and returns its stdout. Panics on a non-zero exit.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args).output().expect("start kestrel");
        assert_success(args, &out);
        String::from_utf8(out.stdout).expect("utf-8 stdout")
    }

    fn serve(&self) -> Serve {
        let config = self.config.to_str().expect("utf-8 path");
        let mut child = self
            .cmd(&["serve", "--config", config, "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start kestrel serve");
        let stdout = child.stdout.take().expect("serve stdout");
        let (tx, rx) = mpsc::channel();
        // Keeps draining stdout so serve never blocks on a full pipe.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut serve = Serve { child };
        let first = rx
            .recv_timeout(SERVE_START)
            .unwrap_or_else(|_| panic!("kestrel serve printed nothing in {SERVE_START:?}"));
        let v: Value = serde_json::from_str(&first)
            .unwrap_or_else(|e| panic!("first serve line is not JSON ({e}): {first}"));
        assert!(
            v["gateway"]
                .as_str()
                .is_some_and(|a| a.parse::<SocketAddr>().is_ok()),
            "first serve line should be {{\"gateway\": \"<ip>:<port>\"}}, got {first}"
        );
        assert!(
            serve.child.try_wait().expect("poll serve").is_none(),
            "serve exited early"
        );
        serve
    }

    /// Writes an agent file with `budget_usd` and returns its path.
    fn agent(&self, budget_usd: &str) -> PathBuf {
        let path = self.dir.path().join(format!("echo-{budget_usd}.yaml"));
        let yaml = format!(
            "name: echo\n\
             runtime: claude-code\n\
             role: Echo test agent\n\
             model_policy:\n  default: tier:cheap\n\
             sandbox:\n  tier: gvisor\n  image: kestrel-echo-agent:test\n  cpu: \"1\"\n  memory: 512Mi\n  workspace: git-worktree\n\
             budget:\n  usd: \"{budget_usd}\"\n  wall_clock: 10m\n"
        );
        std::fs::write(&path, yaml).expect("write agent file");
        path
    }

    /// Submits a run and returns its id.
    fn submit(&self, budget_usd: &str, input: &str) -> String {
        let agent = self.agent(budget_usd);
        let agent = agent.to_str().expect("utf-8 path");
        let out = self.ok(&["run", agent, "--input", input, "--json"]);
        let v: Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("`kestrel run --json` output is not JSON ({e}): {out}"));
        v["run_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no run_id in {out}"))
            .to_string()
    }

    fn row(&self, id: &str) -> Value {
        let out = self.ok(&["ps", "--all", "--json"]);
        let rows: Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("`kestrel ps --json` output is not JSON ({e}): {out}"));
        rows.as_array()
            .and_then(|rows| rows.iter().find(|r| r["id"] == id))
            .cloned()
            .unwrap_or_else(|| panic!("run {id} not in `kestrel ps --all --json`: {out}"))
    }

    /// Polls `ps` until the run's status is `want` (see [`status_name`]). Panics at the deadline
    /// or as soon as the run ends in another status.
    fn wait_status(&self, id: &str, want: &str) -> Value {
        let start = Instant::now();
        loop {
            let row = self.row(id);
            let status = status_name(&row["status"]);
            if status == want {
                return row;
            }
            let terminal = ["Completed", "Failed", "Cancelled"]
                .iter()
                .any(|t| status.starts_with(t));
            if terminal || start.elapsed() >= RUN_DEADLINE {
                // Show what the run did (for example the echo agent's "gateway call failed").
                eprintln!("events of run {id}:\n{}", self.dump_events(id));
            }
            assert!(
                !terminal,
                "run {id} ended as {status}, wanted {want}: {row}"
            );
            assert!(
                start.elapsed() < RUN_DEADLINE,
                "run {id} still {status} after {RUN_DEADLINE:?}, wanted {want}"
            );
            std::thread::sleep(POLL);
        }
    }

    /// The run's workspace: its git worktree of the test repo, on branch `kestrel/run-<id tail>`.
    fn workspace(&self, id: &str) -> PathBuf {
        let tail = &id[id.len().saturating_sub(12)..];
        let out = git(&self.repo, &["worktree", "list", "--porcelain"]);
        let mut path = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/kestrel/run-")
                && b.ends_with(tail)
            {
                return path.expect("worktree line before branch line");
            }
        }
        panic!("no worktree on a kestrel/run-*{tail} branch in the test repo:\n{out}")
    }

    /// The run's events as text, for failure messages. `watch` follows a live run forever, so it
    /// is stopped after a short wait. Never panics.
    fn dump_events(&self, id: &str) -> String {
        let Ok(mut child) = self
            .cmd(&["watch", id, "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        else {
            return "(could not start kestrel watch)".to_string();
        };
        std::thread::sleep(Duration::from_secs(3));
        let _ = child.kill();
        match child.wait_with_output() {
            Ok(out) => format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(e) => format!("(watch failed: {e})"),
        }
    }

    /// Every event of a finished run, through `kestrel watch --json`.
    fn events(&self, id: &str) -> Vec<Value> {
        parse_events(&self.ok(&["watch", id, "--json"]))
    }
}

/// `kestrel serve`, SIGKILLed when dropped.
struct Serve {
    child: Child,
}

impl Serve {
    fn kill(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        self.stop();
    }
}

fn config_toml(upstream: SocketAddr) -> String {
    // The gateway listens on all interfaces: the `kestrel-gateway` bridge container reaches it
    // from the Docker network through the host, not through loopback.
    format!(
        "[gateway]\n\
         listen = \"0.0.0.0:0\"\n\n\
         [engine]\n\
         max_concurrent_runs = 4\n\n\
         [providers.anthropic]\n\
         base_url = \"http://{upstream}\"\n\
         api_key = \"{FAKE_KEY}\"\n\n\
         [models]\n\
         cheap = \"{MODEL}\"\n\
         mid = \"{MODEL}\"\n\
         frontier = \"{MODEL}\"\n\n\
         [prices.\"{MODEL}\"]\n\
         input_per_mtok = \"{PRICE_IN}\"\n\
         output_per_mtok = \"{PRICE_OUT}\"\n\
         cache_read_per_mtok = \"{PRICE_CACHE_READ}\"\n\
         cache_write_per_mtok = \"{PRICE_CACHE_WRITE}\"\n"
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("start git");
    assert_success(args, &out);
    String::from_utf8(out.stdout).expect("utf-8 git output")
}

fn assert_success(args: &[&str], out: &Output) {
    assert!(
        out.status.success(),
        "{args:?} exited with {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A run status from `ps --json` as text: `"Completed"` stays `Completed`,
/// `{"Suspended":"Budget"}` becomes `Suspended(Budget)`, `{"Failed":"x"}` becomes `Failed(x)`.
fn status_name(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(m) if m.len() == 1 => {
            let (k, inner) = m.iter().next().expect("one entry");
            format!("{k}({})", status_name(inner))
        }
        other => other.to_string(),
    }
}

fn parse_events(out: &str) -> Vec<Value> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("`kestrel watch --json` line is not JSON ({e}): {l}"))
        })
        .collect()
}

fn seqs(events: &[Value]) -> Vec<u64> {
    events
        .iter()
        .map(|e| {
            e["seq"]
                .as_u64()
                .unwrap_or_else(|| panic!("event without seq: {e}"))
        })
        .collect()
}

/// The first event whose status changed to `to`.
fn status_event<'a>(events: &'a [Value], to: &str) -> Option<&'a Value> {
    events
        .iter()
        .find(|e| e["kind"]["kind"] == "status_changed" && status_name(&e["kind"]["to"]) == to)
}

fn ts(event: &Value) -> OffsetDateTime {
    let s = event["ts"]
        .as_str()
        .unwrap_or_else(|| panic!("event without ts: {event}"));
    OffsetDateTime::parse(s, &Rfc3339).unwrap_or_else(|e| panic!("bad ts {s}: {e}"))
}

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
