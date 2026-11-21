//! Helpers shared by the end-to-end suites: the mock Anthropic upstream, an isolated Kestrel setup
//! ([`Env`]) around the real `kestrel` binary, and small readers for its output.
//!
//! M1 (`tests/e2e.rs`) uses `kestrel serve`; M2 (`tests/m2.rs`, `tests/ui.rs`) uses `kestrel up`,
//! which adds the HTTP API, and reads the store directly for what the CLI does not print (parents,
//! budgets, capability tokens).

// Each suite uses a different part of this module.
#![allow(dead_code)]
// Helpers panic on purpose: a panic is how a test fails.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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
pub const FAKE_KEY: &str = "sk-ant-e2e-fake-key-3f9a1c7d";
/// The model every tier maps to, priced in the config below.
pub const MODEL: &str = "claude-e2e-1";
/// What the mock reports for every call.
pub const USAGE_IN: u64 = 1000;
pub const USAGE_OUT: u64 = 200;
pub const USAGE_CACHE_READ: u64 = 300;
pub const USAGE_CACHE_WRITE: u64 = 100;
/// Prices in USD per million tokens, as written in the config.
pub const PRICE_IN: &str = "3";
pub const PRICE_OUT: &str = "15";
pub const PRICE_CACHE_READ: &str = "0.30";
pub const PRICE_CACHE_WRITE: &str = "3.75";
/// One call in micro-USD: 1000 * 3 + 200 * 15 + 300 * 0.30 + 100 * 3.75 = 3000 + 3000 + 90 + 375.
pub const ONE_CALL_MICRO_USD: i64 = 6465;
/// README.txt as committed in the test repo. The echo agent appends one line to it.
pub const README_START: &str = "kestrel e2e\n";

pub const POLL: Duration = Duration::from_millis(250);
/// Covers a gVisor cold start on a CI runner with room to spare.
pub const RUN_DEADLINE: Duration = Duration::from_secs(180);
pub const SERVE_START: Duration = Duration::from_secs(60);
/// How long a stopped `kestrel serve` gets to clean up before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(30);

pub fn kestrel_bin() -> PathBuf {
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

pub struct Mock {
    pub addr: SocketAddr,
    seen: Seen,
}

impl Mock {
    /// Starts the mock on its own thread and runtime. It lives until the test process exits.
    pub fn start() -> Mock {
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

    pub fn keys_seen(&self) -> Vec<String> {
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
pub struct Env {
    pub dir: TempDir,
    pub repo: PathBuf,
    pub db: PathBuf,
    pub config: PathBuf,
    pub mock: Mock,
}

impl Env {
    pub fn new() -> Env {
        Env::with_max(4)
    }

    /// An env whose `kestrel serve` runs at most `max_concurrent_runs` sandboxes at once.
    pub fn with_max(max_concurrent_runs: usize) -> Env {
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
        std::fs::write(
            &config,
            config_toml(mock.addr, free_port(), max_concurrent_runs),
        )
        .expect("write config");
        // Private, like a real config: serve warns about a config other users can read.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
                .expect("chmod config");
        }
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
    /// `workspace: git-clone` (repo `.`) resolves to the test repo.
    pub fn cmd(&self, args: &[&str]) -> Command {
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
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd(args).output().expect("start kestrel");
        assert_success(args, &out);
        String::from_utf8(out.stdout).expect("utf-8 stdout")
    }

    /// Starts `kestrel <args> --json` and waits for its first line, which it returns as JSON.
    fn launch(&self, args: &[&str]) -> (Serve, Value) {
        let mut child = self
            .cmd(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start kestrel");
        let stdout = child.stdout.take().expect("kestrel stdout");
        let (tx, rx) = mpsc::channel();
        // Keeps draining stdout so serve never blocks on a full pipe.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut serve = Serve {
            child,
            cleanup: None,
        };
        let first = rx
            .recv_timeout(SERVE_START)
            .unwrap_or_else(|_| panic!("kestrel {args:?} printed nothing in {SERVE_START:?}"));
        let v: Value = serde_json::from_str(&first)
            .unwrap_or_else(|e| panic!("first serve line is not JSON ({e}): {first}"));
        assert!(
            v["gateway"]
                .as_str()
                .is_some_and(|a| a.parse::<SocketAddr>().is_ok()),
            "first serve line should be {{\"gateway\": \"<ip>:<port>\", ...}}, got {first}"
        );
        assert!(
            serve.child.try_wait().expect("poll serve").is_none(),
            "serve exited early"
        );
        (serve, v)
    }

    pub fn serve(&self) -> Serve {
        let config = self.config.to_str().expect("utf-8 path");
        self.launch(&["serve", "--config", config, "--json"]).0
    }

    /// `kestrel up`: serve plus the HTTP API and the embedded UI, on a free loopback port. When the
    /// returned [`Up`] is dropped, serve is stopped with SIGTERM (so it removes its sandboxes and
    /// bridge), and anything of this env still labelled in docker is removed.
    pub fn up(&self) -> Up {
        let config = self.config.to_str().expect("utf-8 path");
        let (mut serve, v) = self.launch(&[
            "up",
            "--config",
            config,
            "--json",
            "--no-open",
            "--listen",
            "127.0.0.1:0",
        ]);
        serve.cleanup = Some(self.instance());
        let api: SocketAddr = v["api"]
            .as_str()
            .and_then(|a| a.parse().ok())
            .unwrap_or_else(|| panic!("`kestrel up --json` printed no api address: {v}"));
        let url = v["url"]
            .as_str()
            .unwrap_or_else(|| panic!("`kestrel up --json` printed no url: {v}"))
            .to_string();
        let token = url
            .split_once("?token=")
            .map(|(_, t)| t.to_string())
            .unwrap_or_else(|| panic!("the UI url carries no token: {url}"));
        assert_eq!(
            token,
            std::fs::read_to_string(self.dir.path().join("server.token"))
                .expect("server.token next to the database")
                .trim(),
            "the printed token is the one in server.token"
        );
        Up {
            serve,
            api,
            token,
            url,
        }
    }

    /// Writes an agent file with `budget_usd` and returns its path.
    pub fn agent(&self, budget_usd: &str) -> PathBuf {
        let path = self.dir.path().join(format!("echo-{budget_usd}.yaml"));
        std::fs::write(
            &path,
            agent_yaml("echo", budget_usd, "mcp: [kestrel.join, kestrel.ask_human]"),
        )
        .expect("write agent file");
        path
    }

    /// Writes `<name>.yaml` in the agents directory next to the database, where `kestrel serve`
    /// looks spawned agents up by name, and returns its path.
    pub fn spawnable(&self, name: &str, budget_usd: &str, capabilities: &str) -> PathBuf {
        let agents = self.dir.path().join("agents");
        std::fs::create_dir_all(&agents).expect("mkdir agents");
        let path = agents.join(format!("{name}.yaml"));
        std::fs::write(&path, agent_yaml(name, budget_usd, capabilities))
            .expect("write agent file");
        path
    }

    /// Writes a root agent file `<name>.yaml` next to the database (not in the agents directory)
    /// and returns its path, for [`Env::submit_file`].
    pub fn root_agent(&self, name: &str, budget_usd: &str, capabilities: &str) -> PathBuf {
        let path = self.dir.path().join(format!("{name}.yaml"));
        std::fs::write(&path, agent_yaml(name, budget_usd, capabilities))
            .expect("write agent file");
        path
    }

    /// Submits a run and returns its id.
    pub fn submit(&self, budget_usd: &str, input: &str) -> String {
        self.submit_file(&self.agent(budget_usd), input)
    }

    /// Submits a run of the agent file at `agent` and returns its id.
    pub fn submit_file(&self, agent: &Path, input: &str) -> String {
        let agent = agent.to_str().expect("utf-8 path");
        let out = self.ok(&["run", agent, "--input", input, "--json"]);
        let v: Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("`kestrel run --json` output is not JSON ({e}): {out}"));
        v["run_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no run_id in {out}"))
            .to_string()
    }

    /// Every run, as `kestrel ps --all --json` prints them.
    pub fn rows(&self) -> Vec<Value> {
        let out = self.ok(&["ps", "--all", "--json"]);
        let rows: Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("`kestrel ps --json` output is not JSON ({e}): {out}"));
        rows.as_array()
            .cloned()
            .unwrap_or_else(|| panic!("`kestrel ps --json` is not a list: {out}"))
    }

    pub fn row(&self, id: &str) -> Value {
        self.rows()
            .into_iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("run {id} not in `kestrel ps --all --json`"))
    }

    /// Polls `ps` until the run's status is `want` (see [`status_name`]). Panics at the deadline
    /// or as soon as the run ends in another status.
    pub fn wait_status(&self, id: &str, want: &str) -> Value {
        self.wait_status_within(id, want, RUN_DEADLINE)
    }

    /// [`Env::wait_status`] with its own deadline.
    pub fn wait_status_within(&self, id: &str, want: &str, deadline: Duration) -> Value {
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
            if terminal || start.elapsed() >= deadline {
                // Show what the run did (for example the echo agent's "gateway call failed").
                eprintln!("events of run {id}:\n{}", self.dump_events(id));
            }
            assert!(
                !terminal,
                "run {id} ended as {status}, wanted {want}: {row}"
            );
            assert!(
                start.elapsed() < deadline,
                "run {id} still {status} after {deadline:?}, wanted {want}"
            );
            std::thread::sleep(POLL);
        }
    }

    /// The run's workspace: its own git clone of the test repo, at `<db dir>/workspaces/<run id>`
    /// (`kestrel serve` keeps workspaces next to the database), on branch `kestrel/run-<id tail>`.
    pub fn workspace(&self, id: &str) -> PathBuf {
        let path = self.dir.path().join("workspaces").join(id);
        assert!(
            path.join(".git").is_dir(),
            "no git clone at {} (a worktree would have a `.git` file)",
            path.display()
        );
        path
    }

    /// The run's events as text, for failure messages. `watch` follows a live run forever, so it
    /// is stopped after a short wait. Never panics.
    pub fn dump_events(&self, id: &str) -> String {
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
    pub fn events(&self, id: &str) -> Vec<Value> {
        parse_events(&self.ok(&["watch", id, "--json"]))
    }

    /// The run's events so far, from the store, in the JSON shape `kestrel watch --json` prints.
    /// Unlike [`Env::events`] it does not wait for the run to end.
    pub fn events_now(&self, id: &str) -> Vec<Value> {
        self.store()
            .events_since(run_id(id), 0)
            .expect("read events")
            .iter()
            .map(|e| serde_json::to_value(e).expect("event as JSON"))
            .collect()
    }

    /// The store `kestrel serve` writes, opened like the CLI opens it. Only after the first
    /// `kestrel run` or serve, which create it.
    pub fn store(&self) -> kestrel_store::Store {
        kestrel_store::Store::open(&self.db).expect("open the store")
    }

    /// The direct children of `id`, oldest first, as the store records them.
    pub fn children(&self, id: &str) -> Vec<kestrel_store::RunRow> {
        self.store().children(run_id(id)).expect("read children")
    }

    /// Waits until `id` has `n` children and returns them, oldest first.
    pub fn wait_children(&self, id: &str, n: usize) -> Vec<kestrel_store::RunRow> {
        let start = Instant::now();
        loop {
            let kids = self.children(id);
            if kids.len() >= n {
                return kids;
            }
            if start.elapsed() >= RUN_DEADLINE {
                eprintln!("events of run {id}:\n{}", self.dump_events(id));
                panic!(
                    "run {id} has {} children after {RUN_DEADLINE:?}, wanted {n}",
                    kids.len()
                );
            }
            std::thread::sleep(POLL);
        }
    }

    /// What `id` has spent, in micro-USD, as the store records it (what `ps` prints).
    pub fn spent(&self, id: &str) -> i64 {
        self.store().spent(run_id(id)).expect("read spend").0
    }

    /// The id `kestrel serve` labels this env's sandboxes and run networks with
    /// (`kestrel.instance`), from the database path, as serve computes it.
    pub fn instance(&self) -> String {
        kestrel_sandbox::GatewayBridge::instance_id(&self.db)
    }

    /// A file the agent wrote in its workspace, or a panic that shows the run's events.
    pub fn read_out(&self, id: &str, name: &str) -> String {
        let path = self.dir.path().join("workspaces").join(id).join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("no {name} ({e}) for run {id}: {}", self.dump_events(id)))
    }
}

/// `kestrel up`, with the API address and token it printed.
pub struct Up {
    pub serve: Serve,
    pub api: SocketAddr,
    pub token: String,
    /// The UI address with the token, as printed: `http://127.0.0.1:<port>/?token=<token>`.
    pub url: String,
}

impl Up {
    /// One request to the API with the bearer token. Returns the status code and the body.
    pub fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        http(self.api, &self.token, method, path, body)
    }

    /// `GET path`, which must answer 200 with JSON.
    pub fn get_json(&self, path: &str) -> Value {
        let (status, body) = self.http("GET", path, None);
        assert_eq!(status, 200, "GET {path}: {body}");
        serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("GET {path} is not JSON ({e}): {body}"))
    }

    /// SIGKILL, like a crash: nothing is cleaned up.
    pub fn kill(self) {
        self.serve.kill();
    }
}

/// A minimal HTTP/1.1 client for the API: `Host` names the bound address, as the server requires,
/// and there is no `Origin`, as for a script.
pub fn http(
    addr: SocketAddr,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, String) {
    let body = body.unwrap_or("");
    let mut stream = std::net::TcpStream::connect(addr).expect("connect to the API");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Accept: application/json\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("{method} {path}: no status line in {head:?}"));
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_string()
    };
    (status, body)
}

/// The body of a `Transfer-Encoding: chunked` response.
fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 || after.len() < size {
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].trim_start_matches("\r\n");
    }
    out
}

/// An echo agent spec named `name` with `budget_usd` and the given `capabilities` body.
pub fn agent_yaml(name: &str, budget_usd: &str, capabilities: &str) -> String {
    format!(
        "name: {name}\n\
         runtime: claude-code\n\
         role: Echo test agent\n\
         model_policy:\n  default: tier:cheap\n\
         capabilities: {{ {capabilities} }}\n\
         sandbox:\n  tier: gvisor\n  image: kestrel-echo-agent:test\n  cpu: \"1\"\n  memory: 512Mi\n  workspace: git-clone\n\
         budget:\n  usd: \"{budget_usd}\"\n  wall_clock: 10m\n"
    )
}

/// `kestrel serve` (or `up`), SIGKILLed when dropped. With `cleanup` (what [`Env::up`] sets) it
/// gets SIGTERM first, so it stops its sandboxes and bridge, and then whatever docker still has
/// labelled with that instance id is removed, so one test leaves nothing running for the next.
pub struct Serve {
    pub child: Child,
    /// The `kestrel.instance` id to clean up after, if any.
    pub cleanup: Option<String>,
}

impl Serve {
    /// SIGKILL, like a crash: nothing is cleaned up, so a restart finds what was left.
    pub fn kill(mut self) {
        self.cleanup = None;
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// SIGTERM, then a wait of up to [`STOP_GRACE`] for serve to stop on its own.
    fn terminate(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let start = Instant::now();
        while start.elapsed() < STOP_GRACE {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        if let Some(instance) = self.cleanup.take() {
            self.terminate();
            self.stop();
            sweep_docker(&instance);
        } else {
            self.stop();
        }
    }
}

/// Removes every container and network labelled `kestrel.instance=<instance>`, and the bridge of
/// that instance. Best effort: never panics.
pub fn sweep_docker(instance: &str) {
    let filter = format!("label=kestrel.instance={instance}");
    let list = |args: &[&str]| -> Vec<String> {
        Command::new("docker")
            .args(args)
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut containers = list(&["ps", "-aq", "--filter", &filter]);
    containers.push(format!("kestrel-gateway-{instance}"));
    for c in containers {
        let _ = Command::new("docker")
            .args(["rm", "--force", &c])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    for n in list(&["network", "ls", "-q", "--filter", &filter]) {
        let _ = Command::new("docker")
            .args(["network", "rm", &n])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A port nothing listens on right now: bind port 0 on loopback and drop the socket. The port is
/// fixed in the config, so a restarted serve (scenario 2) binds the same one again. tokio's
/// `TcpListener::bind` sets SO_REUSEADDR on Unix, so the killed serve's sockets in TIME_WAIT do
/// not block that rebind.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind a free port")
        .local_addr()
        .expect("local addr")
        .port()
}

pub fn config_toml(upstream: SocketAddr, port: u16, max_concurrent_runs: usize) -> String {
    // `gateway.listen` is left out on purpose: serve binds the docker bridge IP it finds with
    // `docker network inspect bridge`, which is the default path the bridge container uses to
    // reach the gateway on the host. Only the port is fixed.
    format!(
        "[gateway]\n\
         port = {port}\n\n\
         [engine]\n\
         max_concurrent_runs = {max_concurrent_runs}\n\n\
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

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("start git");
    assert_success(args, &out);
    String::from_utf8(out.stdout).expect("utf-8 git output")
}

pub fn assert_success(args: &[&str], out: &Output) {
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
pub fn status_name(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(m) if m.len() == 1 => {
            let (k, inner) = m.iter().next().expect("one entry");
            format!("{k}({})", status_name(inner))
        }
        other => other.to_string(),
    }
}

pub fn parse_events(out: &str) -> Vec<Value> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("`kestrel watch --json` line is not JSON ({e}): {l}"))
        })
        .collect()
}

pub fn seqs(events: &[Value]) -> Vec<u64> {
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
pub fn status_event<'a>(events: &'a [Value], to: &str) -> Option<&'a Value> {
    events
        .iter()
        .find(|e| e["kind"]["kind"] == "status_changed" && status_name(&e["kind"]["to"]) == to)
}

/// The events of one kind, such as `model_call`.
pub fn of_kind<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["kind"]["kind"] == kind)
        .collect()
}

pub fn ts(event: &Value) -> OffsetDateTime {
    let s = event["ts"]
        .as_str()
        .unwrap_or_else(|| panic!("event without ts: {event}"));
    OffsetDateTime::parse(s, &Rfc3339).unwrap_or_else(|e| panic!("bad ts {s}: {e}"))
}

pub fn run_id(id: &str) -> kestrel_types::RunId {
    id.parse()
        .unwrap_or_else(|e| panic!("`{id}` is not a run id: {e}"))
}

/// `kestrel/run-<last 12 chars of the id>`, the branch of a run's clone.
pub fn branch_of(id: &str) -> String {
    format!("kestrel/run-{}", &id[id.len().saturating_sub(12)..])
}

// Docker ----------------------------------------------------------------------------------

/// One sandbox container of this env, as `docker ps -a` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// Its `kestrel.run` label: the run id.
    pub run: String,
    /// The networks it is attached to.
    pub networks: Vec<String>,
    /// `running`, `exited`, `created`, ...
    pub state: String,
}

/// The sandbox containers labelled with `instance` (and `kestrel.run`), running or not.
pub fn containers(instance: &str) -> Vec<Container> {
    let out = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=kestrel.instance={instance}"),
            "--filter",
            "label=kestrel.run",
            "--format",
            "{{.Label \"kestrel.run\"}}|{{.Networks}}|{{.State}}",
        ])
        .output()
        .expect("run docker ps");
    assert_success(&["docker", "ps"], &out);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut parts = l.trim().splitn(3, '|');
            let run = parts.next().unwrap_or("").to_string();
            let networks = parts
                .next()
                .unwrap_or("")
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect();
            let state = parts.next().unwrap_or("").to_string();
            Container {
                run,
                networks,
                state,
            }
        })
        .collect()
}

/// The names of the networks labelled with `instance`, as `docker network ls` lists them.
pub fn networks(instance: &str) -> Vec<String> {
    let out = Command::new("docker")
        .args([
            "network",
            "ls",
            "--filter",
            &format!("label=kestrel.instance={instance}"),
            "--format",
            "{{.Name}}",
        ])
        .output()
        .expect("run docker network ls");
    assert_success(&["docker", "network", "ls"], &out);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// A network's subnets and its `kestrel.run` label, or `None` when it is gone.
pub fn network_info(name: &str) -> Option<(Vec<String>, String)> {
    let out = Command::new("docker")
        .args([
            "network",
            "inspect",
            "--format",
            "{{range .IPAM.Config}}{{.Subnet}} {{end}}|{{index .Labels \"kestrel.run\"}}",
            name,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (subnets, run) = text.split_once('|')?;
    Some((
        subnets.split_whitespace().map(str::to_string).collect(),
        run.trim().to_string(),
    ))
}

/// Calls `f` every `every` on a thread of its own until [`Sampler::finish`], and keeps what it
/// returned with the time.
pub struct Sampler<T> {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Vec<(Instant, T)>>>,
}

impl<T: Send + 'static> Sampler<T> {
    pub fn start(every: Duration, mut f: impl FnMut() -> T + Send + 'static) -> Sampler<T> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut samples = Vec::new();
            while !flag.load(Ordering::SeqCst) {
                samples.push((Instant::now(), f()));
                std::thread::sleep(every);
            }
            samples
        });
        Sampler {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops sampling and returns every sample, oldest first.
    pub fn finish(mut self) -> Vec<(Instant, T)> {
        self.stop.store(true, Ordering::SeqCst);
        self.handle
            .take()
            .expect("sampler thread")
            .join()
            .expect("sampler thread panicked")
    }
}

impl<T> Drop for Sampler<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Counts of something by key, for messages.
pub fn tally<'a>(items: impl IntoIterator<Item = &'a str>) -> BTreeMap<&'a str, usize> {
    let mut out = BTreeMap::new();
    for item in items {
        *out.entry(item).or_insert(0) += 1;
    }
    out
}
