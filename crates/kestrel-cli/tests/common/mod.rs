//! Shared helpers for the `kestrel` CLI scenario tests. Every test gets its own temp dir and
//! passes `--db` explicitly, so nothing touches the real user data dir.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the right
// way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used, dead_code)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use kestrel_store::Store;
use kestrel_types::{AgentSpec, RunId, RunStatus};
use tempfile::TempDir;

/// A valid agent spec, shaped like the example in `docs/architecture.md` section 3.1.
pub const GOOD_YAML: &str = r#"
apiVersion: kestrel/v1
kind: Agent
name: cli-tester
runtime: claude-code
role: tester
model_policy: { default: tier:mid }
sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: git-worktree }
budget: { usd: 2.00, wall_clock: 10m }
"#;

/// How long any poll waits before the test fails. Generous for slow CI; polls return as soon
/// as the condition holds, so a passing test never waits this long.
pub const DEADLINE: Duration = Duration::from_secs(20);
/// Poll step.
pub const STEP: Duration = Duration::from_millis(25);

/// A temp data dir with a database path inside it.
pub struct Fixture {
    pub dir: TempDir,
    pub db: PathBuf,
}

impl Fixture {
    pub fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("kestrel.db");
        Fixture { dir, db }
    }

    /// A second handle on the same database, as another process would hold.
    pub fn store(&self) -> Store {
        Store::open(&self.db).expect("open store")
    }

    /// `kestrel --db <db>` ready for more args.
    pub fn kestrel(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("kestrel").expect("kestrel binary");
        cmd.arg("--db").arg(&self.db);
        cmd
    }

    /// Same as [`Fixture::kestrel`] as a plain `std::process::Command`, for child processes the
    /// test talks to while they run.
    pub fn kestrel_std(&self) -> StdCommand {
        let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin("kestrel"));
        cmd.arg("--db").arg(&self.db);
        cmd
    }

    /// Writes `contents` to `name` inside the temp dir and returns the path.
    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, contents).expect("write fixture file");
        path
    }

    pub fn good_agent(&self) -> PathBuf {
        self.write("good.yaml", GOOD_YAML)
    }
}

pub fn agent() -> AgentSpec {
    AgentSpec::from_yaml(GOOD_YAML).expect("fixture agent parses")
}

/// Moves a run along `path`, starting from its current status `from`.
pub fn walk(store: &Store, id: RunId, from: RunStatus, path: &[RunStatus]) {
    let mut cur = from;
    for next in path {
        store
            .transition(id, &cur, next.clone())
            .unwrap_or_else(|e| panic!("transition {cur:?} -> {next:?}: {e}"));
        cur = next.clone();
    }
}

/// A run in `Running`, created directly through the store.
pub fn running_run(store: &Store) -> RunId {
    let id = store.create_run(&agent(), "task").expect("create run");
    walk(
        store,
        id,
        RunStatus::Queued,
        &[RunStatus::Provisioning, RunStatus::Running],
    );
    id
}

/// Polls `f` until it returns `Some`, or panics with `what` after [`DEADLINE`].
pub fn poll<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + DEADLINE;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(STEP);
    }
}

/// A child process killed on drop, so a failing test never leaks one.
pub struct ChildGuard {
    pub child: Child,
}

impl ChildGuard {
    /// Spawns `cmd` with piped stdout and stderr.
    pub fn spawn(mut cmd: StdCommand) -> ChildGuard {
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kestrel");
        ChildGuard { child }
    }

    /// Stdout as a stream of lines, read on a background thread so the test never blocks.
    pub fn stdout_lines(&mut self) -> Lines {
        let out = self.child.stdout.take().expect("stdout piped");
        Lines::new(out)
    }

    /// Everything on stderr (read after the child exits).
    pub fn stderr(&mut self) -> String {
        let mut s = String::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_string(&mut s);
        }
        s
    }

    /// Waits for exit, or panics after [`DEADLINE`].
    pub fn wait_exit(&mut self, what: &str) -> std::process::ExitStatus {
        poll(what, || self.child.try_wait().expect("try_wait"))
    }

    /// Sends SIGINT, the way Ctrl-C in a terminal does.
    pub fn interrupt(&self) {
        let status = StdCommand::new("kill")
            .arg("-s")
            .arg("INT")
            .arg(self.child.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -s INT failed");
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Lines from a child's stdout, delivered through a channel.
pub struct Lines {
    rx: mpsc::Receiver<String>,
    /// Every line received so far, for failure messages.
    pub seen: Vec<String>,
}

impl Lines {
    fn new(out: impl Read + Send + 'static) -> Lines {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Lines {
            rx,
            seen: Vec::new(),
        }
    }

    /// The next line, or panics after [`DEADLINE`].
    pub fn next_line(&mut self, what: &str) -> String {
        match self.rx.recv_timeout(DEADLINE) {
            Ok(line) => {
                self.seen.push(line.clone());
                line
            }
            Err(_) => panic!(
                "timed out waiting for {what}; stdout so far: {:#?}",
                self.seen
            ),
        }
    }

    /// Reads lines until one satisfies `pred`; returns it.
    pub fn until(&mut self, what: &str, pred: impl Fn(&str) -> bool) -> String {
        let end = Instant::now() + DEADLINE;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if pred(&line) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "timed out waiting for {what}; stdout so far: {:#?}",
                    self.seen
                ),
            }
        }
    }

    /// Lines still buffered right now, without waiting.
    pub fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(line) = self.rx.try_recv() {
            self.seen.push(line.clone());
            out.push(line);
        }
        out
    }

    /// Every remaining line until the child closes stdout (or [`DEADLINE`]).
    pub fn rest(&mut self) -> Vec<String> {
        let end = Instant::now() + DEADLINE;
        let mut out = Vec::new();
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    out.push(line);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return out,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("stdout never closed; so far: {:#?}", self.seen)
                }
            }
        }
    }
}

/// Parses one stdout line of `--json` output.
pub fn json_line(line: &str) -> serde_json::Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
}

/// Parses all of a command's stdout as one JSON document.
pub fn json_doc(stdout: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

pub fn path_str(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}
