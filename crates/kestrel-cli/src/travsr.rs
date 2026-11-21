//! Travsr for `serve` (#44), when `kestrel.toml` has a `[travsr]` section.
//!
//! Travsr runs on the host, as Kestrel's user, so it never touches a run's workspace: the agent
//! may rewrite anything there, and Travsr would read it, run git in it and serve files by the
//! paths its index names. Instead:
//!
//! * [`TravsrProcesses`] keeps a private copy of each indexed run's code at
//!   `<data dir>/travsr/<run id>/repo`: a clone of the run's source repo at the commit its
//!   workspace was cloned at (`kestrel_core::workspace::clone_at`). The `travsr` directory and
//!   each run's directory are owner-only (0700), and no sandbox ever mounts them.
//! * It indexes that copy with `travsr init --no-connect`, with `CI=1` (no background daemon),
//!   `TRAVSR_SKIP_DOWNLOAD=1` (no tool or model download) and `TRAVSR_DISABLE_REGISTRY=1` (no
//!   entry in Travsr's global registry), and records the commit, so a resumed run reuses or
//!   remakes the same copy.
//! * Each run that calls a tool gets its own `travsr mcp --stdio --db <copy>/.travsr/graph.db`,
//!   started on its first call in the copy, once the database is checked to be a regular file
//!   inside the run's directory.
//! * Forgetting a run stops its server, runs `travsr repos --remove <copy>` and deletes the run's
//!   directory, each bounded in time.
//! * [`EngineTravsr`] is what the gateway asks: which tools a run may call (from the engine) and
//!   the calls themselves, always to the run's own server.
//!
//! Travsr gets a short list of environment variables (see [`PASSED_ENV`] and [`SET_ENV`]), never
//! the run token or a provider key.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use kestrel_core::{Engine, IndexSource, RunIndexer};
use kestrel_gateway::{ToolsError, TravsrHost};
use kestrel_sandbox::CommandRunner;
use kestrel_types::RunId;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// The `[travsr]` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TravsrSettings {
    /// The travsr command: a path, or a name looked up on `PATH`.
    pub(crate) command: PathBuf,
    /// The longest indexing of one run may take.
    pub(crate) index_timeout: Duration,
    /// The longest one tool call may take.
    pub(crate) call_timeout: Duration,
    /// Most runs indexed at once.
    pub(crate) max_indexing: usize,
}

impl Default for TravsrSettings {
    fn default() -> Self {
        Self {
            command: PathBuf::from("travsr"),
            index_timeout: Duration::from_secs(300),
            call_timeout: Duration::from_secs(30),
            max_indexing: 2,
        }
    }
}

/// Environment variables Travsr gets from Kestrel's: where things are, never a secret. Any other
/// `TRAVSR_*` is not passed: some of Travsr's are keys.
const PASSED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "SystemRoot",
    // Where Travsr's language tools and its language settings live.
    "TRAVSR_BIN_DIR",
    "TRAVSR_LANG_TOML",
];

/// Set on every Travsr command, whatever Kestrel's environment says.
///
/// * `CI`: `travsr init` and `travsr mcp` start no background daemon (and an interrupted init
///   hands nothing to one).
/// * `TRAVSR_SKIP_DOWNLOAD`: `travsr init` downloads no language tool and no ranking model.
/// * `TRAVSR_DISABLE_REGISTRY`: the copy is not added to Travsr's global registry.
const SET_ENV: &[(&str, &str)] = &[
    ("CI", "1"),
    ("TRAVSR_SKIP_DOWNLOAD", "1"),
    ("TRAVSR_DISABLE_REGISTRY", "1"),
];

/// Most bytes of a failed command's output kept for its error.
const TAIL: usize = 500;
/// Longest line read from a Travsr server; a longer answer is an error.
const MAX_LINE: u64 = 16 * 1024 * 1024;
/// The longest `travsr repos --remove`, or a wait for a call to give a run's server up, may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether Kestrel's variable `name` is passed to Travsr. Windows names are not case sensitive
/// (`SystemRoot` may be `SYSTEMROOT`), Unix ones are.
fn passed(name: &str) -> bool {
    PASSED_ENV.iter().any(|p| {
        if cfg!(windows) {
            p.eq_ignore_ascii_case(name)
        } else {
            *p == name
        }
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One run's Travsr server, `travsr mcp --stdio`, speaking newline-delimited JSON-RPC.
struct Server {
    /// Killed when dropped.
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// A request was sent and its answer not read: a call was dropped halfway, so the next one
    /// cannot trust what comes next.
    busy: bool,
}

/// Why a request got no answer: the server is no good any more.
struct Broken(String);

impl Server {
    fn dead(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    /// Sends one request and reads lines until its answer. `Ok(Err)` is Travsr's JSON-RPC error.
    async fn ask(&mut self, method: &str, params: Value) -> Result<Result<Value, String>, Broken> {
        self.next_id += 1;
        let id = self.next_id;
        // The id first, so a plain-text reader finds it at the start of the line.
        let line = format!(
            "{{\"id\":{id},\"jsonrpc\":\"2.0\",\"method\":{},\"params\":{params}}}\n",
            json!(method)
        );
        self.busy = true;
        let stopped = |e: io::Error| Broken(format!("Travsr stopped: {e}"));
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(stopped)?;
        self.stdin.flush().await.map_err(stopped)?;
        loop {
            let mut buf = String::new();
            let read = (&mut self.stdout)
                .take(MAX_LINE)
                .read_line(&mut buf)
                .await
                .map_err(stopped)?;
            if read == 0 {
                return Err(Broken("Travsr stopped without answering".into()));
            }
            if !buf.ends_with('\n') && read as u64 >= MAX_LINE {
                return Err(Broken("Travsr's answer is larger than 16 MiB".into()));
            }
            // Log lines, notifications and answers to calls given up on are skipped.
            let Ok(message) = serde_json::from_str::<Value>(&buf) else {
                continue;
            };
            if message.get("id") != Some(&json!(id)) {
                continue;
            }
            self.busy = false;
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| error.to_string(), str::to_string);
                return Ok(Err(text));
            }
            return Ok(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
        }
    }
}

/// One run's server, and whether the run was stopped meanwhile.
#[derive(Default)]
struct Held {
    server: Option<Server>,
    /// The run is no longer served: a call that waited for the server starts none.
    closed: bool,
}

/// One run's server, behind a lock: a run's calls go one at a time.
type Slot = Arc<tokio::sync::Mutex<Held>>;

/// Which runs are served, and their servers.
#[derive(Default)]
struct Served {
    /// Runs indexed and not released or forgotten since.
    runs: HashSet<RunId>,
    servers: HashMap<RunId, Slot>,
}

/// Runs the travsr command for the engine and the gateway, on private copies under `root`.
pub(crate) struct TravsrProcesses {
    command: PathBuf,
    /// `<data dir>/travsr`: one owner-only directory per run, named by its id.
    root: PathBuf,
    /// Runs git for the copies.
    git: Arc<dyn CommandRunner>,
    /// The longest one call to a server may take.
    call_timeout: Duration,
    /// The longest `repos --remove`, or a wait for a call to give a server up, may take.
    command_timeout: Duration,
    served: Mutex<Served>,
}

impl TravsrProcesses {
    pub(crate) fn new(
        command: PathBuf,
        root: PathBuf,
        git: Arc<dyn CommandRunner>,
        call_timeout: Duration,
    ) -> TravsrProcesses {
        TravsrProcesses {
            command,
            root,
            git,
            call_timeout,
            command_timeout: COMMAND_TIMEOUT,
            served: Mutex::default(),
        }
    }

    /// `<root>/<run id>`: everything Kestrel keeps for the run.
    fn dir(&self, run: RunId) -> PathBuf {
        self.root.join(run.to_string())
    }

    /// The run's private copy, which Travsr indexes and serves.
    fn copy(&self, run: RunId) -> PathBuf {
        self.dir(run).join("repo")
    }

    /// The travsr command with `args`, a clean environment and no stdin. Killed when dropped.
    fn command<S: AsRef<OsStr>>(&self, args: impl IntoIterator<Item = S>) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.args(args)
            .env_clear()
            .envs(std::env::vars_os().filter(|(name, _)| name.to_str().is_some_and(passed)))
            .envs(SET_ENV.iter().copied())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }

    /// Runs `cmd` to the end, at most `limit` when there is one. `Err` names `what` and ends with
    /// the tail of its output.
    async fn run(
        &self,
        mut cmd: Command,
        what: &str,
        limit: Option<Duration>,
    ) -> Result<(), String> {
        let output = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output();
        let out = match limit {
            Some(limit) => tokio::time::timeout(limit, output).await.map_err(|_| {
                format!(
                    "`travsr {what}` did not finish within {} s",
                    limit.as_secs()
                )
            })?,
            None => output.await,
        }
        .map_err(|e| format!("could not start {}: {e}", self.command.display()))?;
        if out.status.success() {
            return Ok(());
        }
        let said = if out.stderr.iter().all(u8::is_ascii_whitespace) {
            &out.stdout
        } else {
            &out.stderr
        };
        let text = String::from_utf8_lossy(said);
        let text = text.trim();
        let mut start = text.len().saturating_sub(TAIL);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let code = out
            .status
            .code()
            .map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
        Err(format!(
            "`travsr {what}` exited with {code}: {}",
            &text[start..]
        ))
    }

    /// `run`'s slot, made if it has none; `None` when the run is not served.
    fn slot(&self, run: RunId) -> Option<Slot> {
        let mut served = lock(&self.served);
        if !served.runs.contains(&run) {
            return None;
        }
        Some(served.servers.entry(run).or_default().clone())
    }

    /// Stops serving `run`: no new call reaches it, and its server is stopped. A call under way
    /// keeps the server at most `command_timeout`; past that, the server goes when that call
    /// ends (at most its own timeout later).
    async fn stop(&self, run: RunId) {
        let slot = {
            let mut served = lock(&self.served);
            served.runs.remove(&run);
            served.servers.remove(&run)
        };
        if let Some(slot) = slot
            && let Ok(mut held) = tokio::time::timeout(self.command_timeout, slot.lock()).await
        {
            held.closed = true;
            held.server.take();
        }
    }

    /// Asks `run`'s server, starting it if needed, at most `call_timeout`. A server that died,
    /// that a dropped call left halfway, or that did not answer in time, is started again on the
    /// next call.
    pub(crate) async fn request(
        &self,
        run: RunId,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let unavailable = || "Travsr is not available for this run".to_string();
        let slot = self.slot(run).ok_or_else(unavailable)?;
        let mut held = slot.lock().await;
        if held.closed {
            return Err(unavailable());
        }
        if held.server.as_mut().is_some_and(|s| s.busy || s.dead()) {
            held.server.take();
        }
        let server = match held.server.as_mut() {
            Some(server) => server,
            None => held.server.insert(self.start(run)?),
        };
        let limit = self.call_timeout;
        match tokio::time::timeout(limit, server.ask(method, params)).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(Broken(why))) => {
                held.server.take();
                Err(why)
            }
            Err(_) => {
                held.server.take();
                Err(format!(
                    "Travsr did not answer within {} s",
                    limit.as_secs()
                ))
            }
        }
    }

    /// Starts a server for `run`'s copy, once its database is checked.
    fn start(&self, run: RunId) -> Result<Server, String> {
        let db = self.checked_db(run)?;
        let mut child = self
            .command([OsStr::new("mcp"), OsStr::new("--stdio"), OsStr::new("--db")])
            .arg(&db)
            .current_dir(self.copy(run))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", self.command.display()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err("the Travsr server has no stdin or stdout".into());
        };
        Ok(Server {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            busy: false,
        })
    }

    /// `run`'s index database, once it is a regular file reached through real directories
    /// only, inside the run's directory: nothing a link could point elsewhere.
    fn checked_db(&self, run: RunId) -> Result<PathBuf, String> {
        let dir = self.dir(run);
        let index = self.copy(run).join(".travsr");
        let db = index.join("graph.db");
        for step in [&self.root, &dir, &self.copy(run), &index] {
            if !std::fs::symlink_metadata(step).is_ok_and(|m| m.is_dir()) {
                return Err(format!(
                    "Travsr's index is not usable: {} is not a directory",
                    step.display()
                ));
            }
        }
        if !std::fs::symlink_metadata(&db).is_ok_and(|m| m.is_file()) {
            return Err(format!(
                "Travsr's index is not usable: {} is not a regular file",
                db.display()
            ));
        }
        let inside = match (std::fs::canonicalize(&db), std::fs::canonicalize(&dir)) {
            (Ok(db), Ok(dir)) => db.starts_with(dir),
            _ => false,
        };
        if !inside {
            return Err(format!(
                "Travsr's index is not usable: {} is not inside {}",
                db.display(),
                dir.display()
            ));
        }
        Ok(db)
    }

    /// Makes `<root>/<run id>`, owner-only, and returns it. `root` is made too, owner-only.
    fn private_dir(&self, run: RunId) -> io::Result<PathBuf> {
        if let Some(parent) = self.root.parent() {
            std::fs::create_dir_all(parent)?;
        }
        owner_only_dir(&self.root)?;
        let dir = self.dir(run);
        owner_only_dir(&dir)?;
        Ok(dir)
    }

    /// Indexes `run`'s copy of `source`; see [`RunIndexer::index`].
    async fn index_copy(&self, run: RunId, source: &IndexSource) -> Result<(), String> {
        let dir = self
            .private_dir(run)
            .map_err(|e| format!("could not make Travsr's directory for this run: {e}"))?;
        let (commit_file, done, copy) = (dir.join("commit"), dir.join("indexed"), self.copy(run));
        let recorded = read_if_any(&commit_file)
            .map_err(|e| format!("could not read the recorded commit: {e}"))?;
        let commit = match (&source.commit, recorded) {
            (Some(commit), Some(recorded)) if *commit == recorded => recorded,
            (Some(commit), _) => {
                // New, or not the copy recorded: start over.
                remove_if_any(&done).map_err(|e| format!("could not reset the copy: {e}"))?;
                remove_dir_if_any(&copy)
                    .await
                    .map_err(|e| format!("could not reset the copy: {e}"))?;
                std::fs::write(&commit_file, commit)
                    .map_err(|e| format!("could not record the commit: {e}"))?;
                commit.clone()
            }
            (None, Some(recorded)) => recorded,
            (None, None) => {
                return Err(
                    "the commit this run started from was never recorded, so its code cannot be \
                     copied"
                        .into(),
                );
            }
        };
        // Indexed before, and still whole: reused as it is.
        if done.exists() && self.checked_db(run).is_ok() {
            return Ok(());
        }
        remove_if_any(&done).map_err(|e| format!("could not reset the copy: {e}"))?;
        remove_dir_if_any(&copy)
            .await
            .map_err(|e| format!("could not reset the copy: {e}"))?;
        kestrel_core::workspace::clone_at(self.git.as_ref(), &source.repo, &commit, &copy)
            .await
            .map_err(|e| format!("could not copy the run's code: {e}"))?;
        let mut init = self.command(["init", "--no-connect"]);
        init.current_dir(&copy);
        self.run(init, "init", None).await?;
        self.checked_db(run)?;
        std::fs::write(&done, "").map_err(|e| format!("could not mark the copy indexed: {e}"))
    }
}

/// Makes `path` a directory only its owner can open, unless it is one already. Refuses a path
/// that is a link or a file.
fn owner_only_dir(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                path.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            let builder = {
                let mut builder = std::fs::DirBuilder::new();
                std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
                builder
            };
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            builder.create(path)?;
        }
        Err(e) => return Err(e),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The trimmed text of a small file, `None` when there is none.
fn read_if_any(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn remove_if_any(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

async fn remove_dir_if_any(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[async_trait]
impl RunIndexer for TravsrProcesses {
    async fn index(&self, run: RunId, source: &IndexSource) -> Result<(), String> {
        // A server of an earlier index would not see the new one.
        self.stop(run).await;
        self.index_copy(run, source).await?;
        lock(&self.served).runs.insert(run);
        Ok(())
    }

    async fn release(&self, run: RunId) {
        self.stop(run).await;
    }

    async fn forget(&self, run: RunId) -> Result<(), String> {
        self.stop(run).await;
        // As `travsr init` would have registered it: by its canonical path. Nothing is
        // registered with `TRAVSR_DISABLE_REGISTRY`; this removes what an older setup left, and
        // Travsr exits 0 for a path it does not know.
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let copy = root.join(run.to_string()).join("repo");
        let remove = self.command([
            OsStr::new("repos"),
            OsStr::new("--remove"),
            copy.as_os_str(),
        ]);
        let removed = self
            .run(remove, "repos --remove", Some(self.command_timeout))
            .await;
        // The copy goes whatever Travsr said.
        let deleted = remove_dir_if_any(&self.dir(run))
            .await
            .map_err(|e| format!("could not delete {}: {e}", self.dir(run).display()));
        removed.and(deleted)
    }

    async fn kept(&self) -> Result<Vec<RunId>, String> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("could not list {}: {e}", self.root.display())),
        };
        let mut runs = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("could not list {}: {e}", self.root.display()))?;
            if let Some(run) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
                runs.push(run);
            }
        }
        Ok(runs)
    }
}

/// The gateway's Travsr host: the engine says what a run may call, the processes answer.
pub(crate) struct EngineTravsr {
    pub(crate) engine: Arc<Engine>,
    pub(crate) processes: Arc<TravsrProcesses>,
    pub(crate) call_timeout: Duration,
}

#[async_trait]
impl TravsrHost for EngineTravsr {
    async fn tools(&self, run: RunId) -> Result<Vec<String>, ToolsError> {
        match self.engine.travsr_tools(run) {
            Ok(Some(tools)) => Ok(tools),
            Ok(None) => Err(ToolsError::Gone),
            Err(_) => Err(ToolsError::Unavailable),
        }
    }

    async fn schemas(&self, run: RunId) -> Result<Vec<Value>, String> {
        let listed = self.processes.request(run, "tools/list", json!({})).await?;
        listed
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| "Travsr listed no tools".to_string())
    }

    async fn call(&self, run: RunId, tool: &str, args: Value) -> Result<Value, String> {
        let params = json!({ "name": tool, "arguments": args });
        self.processes.request(run, "tools/call", params).await
    }

    async fn denied(&self, run: RunId, tool: &str) {
        if let Err(e) = self.engine.tool_denied(run, tool) {
            eprintln!("kestrel: recording a refused {tool} call for run {run}: {e}");
        }
    }

    fn call_timeout(&self) -> Duration {
        self.call_timeout
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn the_defaults_are_travsr_on_path_5_minutes_30_seconds_and_2_jobs() {
        let d = TravsrSettings::default();
        assert_eq!(d.command, PathBuf::from("travsr"));
        assert_eq!(d.index_timeout, Duration::from_secs(300));
        assert_eq!(d.call_timeout, Duration::from_secs(30));
        assert_eq!(d.max_indexing, 2);
    }

    #[test]
    fn only_locations_reach_travsr_and_windows_names_ignore_case() {
        for name in [
            "PATH",
            "HOME",
            "TRAVSR_BIN_DIR",
            "TRAVSR_LANG_TOML",
            "SystemRoot",
        ] {
            assert!(passed(name), "{name}");
        }
        for name in [
            "ANTHROPIC_API_KEY",
            "KESTREL_RUN_TOKEN",
            "TRAVSR_SIGNING_KEY_HEX",
            "TRAVSR_ALLOW_UNSANDBOXED",
            "CI",
            "GIT_DIR",
        ] {
            assert!(!passed(name), "{name}");
        }
        assert_eq!(passed("SYSTEMROOT"), cfg!(windows));
        assert_eq!(passed("path"), cfg!(windows));
        // Set whatever Kestrel's environment says.
        assert_eq!(
            SET_ENV,
            [
                ("CI", "1"),
                ("TRAVSR_SKIP_DOWNLOAD", "1"),
                ("TRAVSR_DISABLE_REGISTRY", "1")
            ]
        );
    }

    /// Git whose `clone` makes the target directory with a `.git` directory, and whose
    /// `rev-parse` answers [`SHA`].
    struct FakeGit;

    #[async_trait]
    impl CommandRunner for FakeGit {
        async fn run(&self, argv: &[String]) -> io::Result<kestrel_sandbox::CommandOutput> {
            if argv.get(1).is_some_and(|a| a == "clone") {
                std::fs::create_dir_all(Path::new(argv.last().unwrap()).join(".git"))?;
            }
            let stdout = if argv.iter().any(|a| a == "HEAD^{commit}") {
                format!("{SHA}\n")
            } else {
                ".git\n".into()
            };
            Ok(kestrel_sandbox::CommandOutput {
                status: 0,
                stdout,
                stderr: String::new(),
            })
        }
        async fn spawn(&self, _: &[String]) -> io::Result<kestrel_sandbox::SpawnedProcess> {
            Err(io::Error::other("never spawned"))
        }
    }

    fn at(repo: &Path, commit: Option<&str>) -> IndexSource {
        IndexSource {
            repo: repo.to_path_buf(),
            commit: commit.map(str::to_string),
        }
    }

    /// Processes on a missing travsr command, under `<dir>/travsr`.
    fn missing(dir: &Path) -> TravsrProcesses {
        TravsrProcesses::new(
            dir.join("no-such-travsr"),
            dir.join("travsr"),
            Arc::new(FakeGit),
            Duration::from_secs(5),
        )
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = missing(dir.path());
        let run = RunId::new();
        let err = p.index(run, &at(dir.path(), Some(SHA))).await.unwrap_err();
        assert!(
            err.contains("could not start") && err.contains("no-such-travsr"),
            "{err}"
        );
        // Not served: no call reaches anything.
        let err = p.request(run, "tools/list", json!({})).await.unwrap_err();
        assert!(err.contains("not available"), "{err}");
        // Forgetting still deletes the copy, and says Travsr could not be asked.
        assert!(p.dir(run).exists());
        let err = p.forget(run).await.unwrap_err();
        assert!(err.contains("could not start"), "{err}");
        assert!(!p.dir(run).exists());
    }

    #[tokio::test]
    async fn a_resumed_run_with_no_recorded_commit_is_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let p = missing(dir.path());
        let err = p
            .index(RunId::new(), &at(dir.path(), None))
            .await
            .unwrap_err();
        assert!(err.contains("never recorded"), "{err}");
    }

    #[tokio::test]
    async fn the_commit_is_recorded_once_and_a_different_one_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let p = missing(dir.path());
        let run = RunId::new();
        let _ = p.index(run, &at(dir.path(), Some(SHA))).await;
        let commit = p.dir(run).join("commit");
        assert_eq!(std::fs::read_to_string(&commit).unwrap(), SHA);
        // A copy marked indexed under another commit is reset.
        std::fs::write(p.dir(run).join("indexed"), "").unwrap();
        let other = "f".repeat(40);
        let _ = p.index(run, &at(dir.path(), Some(&other))).await;
        assert_eq!(std::fs::read_to_string(&commit).unwrap(), other);
        assert!(!p.dir(run).join("indexed").exists());
        // A resumed run uses the commit recorded.
        let err = p.index(run, &at(dir.path(), None)).await.unwrap_err();
        assert!(err.contains("could not start"), "{err}");
    }

    #[tokio::test]
    async fn kept_lists_the_runs_with_a_directory_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let p = missing(dir.path());
        assert_eq!(p.kept().await.unwrap(), Vec::new());
        let (a, b) = (RunId::new(), RunId::new());
        p.private_dir(a).unwrap();
        p.private_dir(b).unwrap();
        std::fs::create_dir_all(dir.path().join("travsr").join("notes")).unwrap();
        let mut kept = p.kept().await.unwrap();
        kept.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(kept, want);
        // A root that is a file cannot be listed.
        let file = TravsrProcesses::new(
            dir.path().join("t"),
            dir.path().join("travsr").join(a.to_string()).join("x"),
            Arc::new(FakeGit),
            Duration::from_secs(1),
        );
        std::fs::write(&file.root, "").unwrap();
        assert!(file.kept().await.is_err());
        assert!(file.private_dir(a).is_err());
    }

    #[test]
    fn the_index_must_be_a_regular_file_in_real_directories() {
        let dir = tempfile::tempdir().unwrap();
        let p = missing(dir.path());
        let run = RunId::new();
        let err = p.checked_db(run).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        let index = p.copy(run).join(".travsr");
        std::fs::create_dir_all(&index).unwrap();
        let err = p.checked_db(run).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        std::fs::create_dir_all(index.join("graph.db")).unwrap();
        assert!(p.checked_db(run).is_err());
        std::fs::remove_dir(index.join("graph.db")).unwrap();
        std::fs::write(index.join("graph.db"), "").unwrap();
        assert_eq!(p.checked_db(run).unwrap(), index.join("graph.db"));
    }

    #[cfg(unix)]
    #[test]
    fn an_index_reached_through_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = missing(dir.path());
        let run = RunId::new();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join(".travsr")).unwrap();
        std::fs::write(elsewhere.join(".travsr/graph.db"), "").unwrap();
        // The database a link to another run's, or any, file.
        let index = p.copy(run).join(".travsr");
        std::fs::create_dir_all(&index).unwrap();
        std::os::unix::fs::symlink(elsewhere.join(".travsr/graph.db"), index.join("graph.db"))
            .unwrap();
        let err = p.checked_db(run).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        // `.travsr`, or the copy itself, a link to a directory.
        std::fs::remove_dir_all(&index).unwrap();
        std::os::unix::fs::symlink(elsewhere.join(".travsr"), &index).unwrap();
        assert!(p.checked_db(run).unwrap_err().contains("not a directory"));
        std::fs::remove_dir_all(p.dir(run)).unwrap();
        std::fs::create_dir_all(p.dir(run)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, p.copy(run)).unwrap();
        assert!(p.checked_db(run).unwrap_err().contains("not a directory"));
    }

    #[cfg(unix)]
    #[test]
    fn the_private_directories_are_owner_only_and_never_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = missing(dir.path());
        let run = RunId::new();
        let made = p.private_dir(run).unwrap();
        for d in [&p.root, &made] {
            let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", d.display());
        }
        // An existing directory is made owner-only too.
        std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.private_dir(run).unwrap();
        let mode = std::fs::metadata(&made).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // A run directory that is a link is refused.
        let other = RunId::new();
        std::os::unix::fs::symlink(dir.path(), p.dir(other)).unwrap();
        assert!(p.private_dir(other).is_err());
    }

    /// A fake `travsr` (a shell script) that behaves like the real one where Kestrel's safety
    /// depends on it, and writes down every call.
    #[cfg(unix)]
    pub(crate) mod fake {
        use std::path::{Path, PathBuf};

        /// Every call appends `argv <args>` and `cwd <dir>` to `<dir>/log` and writes its
        /// environment to `<dir>/env.<n>`, `n` counting calls from 1.
        ///
        /// Like the real Travsr:
        /// * `init` fails outside a git repo; writes `.travsr/graph.db` and a git hook; without
        ///   `--no-connect` writes `.mcp.json`, `CLAUDE.md` and `AGENTS.md`; without `CI` starts a
        ///   daemon (a line in `<dir>/daemons`); without `TRAVSR_SKIP_DOWNLOAD` downloads (a line
        ///   in `<dir>/downloads`); without `TRAVSR_DISABLE_REGISTRY=1` registers the repo (a
        ///   line in `<dir>/registry`).
        /// * `repos --remove <path>` exits 0 whether or not the path is registered.
        /// * `mcp` starts a daemon without `CI`, then answers `tools/list`, echoes every
        ///   `tools/call` as its text, and for tools named `die`, `quit`, `hang` and `bad` dies,
        ///   answers then quits, never answers, or answers with an error.
        ///
        /// Flag files in `<dir>`: `init_fails`, `init_sleeps`, `remove_fails`, `remove_hangs`.
        const SCRIPT: &str = r#"#!/bin/sh
d='@DIR@'
echo "argv $*" >> "$d/log"
echo "cwd $(pwd -P)" >> "$d/log"
n=$(grep -c '^argv ' "$d/log")
env > "$d/env.$n"
daemon() { if [ -z "$CI" ]; then echo "daemon in $(pwd -P)" >> "$d/daemons"; fi; }
case "$1" in
  init)
    [ -d .git ] || { echo "error: not a git repository" >&2; exit 2; }
    if [ -f "$d/init_fails" ]; then echo "indexing 3 files"; echo "boom: no space left" >&2; exit 3; fi
    if [ -f "$d/init_sleeps" ]; then sleep 30; fi
    [ -n "$TRAVSR_SKIP_DOWNLOAD" ] || echo "download in $(pwd -P)" >> "$d/downloads"
    [ "$TRAVSR_DISABLE_REGISTRY" = 1 ] || echo "$(pwd -P)" >> "$d/registry"
    mkdir -p .travsr .git/hooks && : > .travsr/graph.db
    echo 'travsr hook-run' > .git/hooks/post-commit
    case " $* " in
      *" --no-connect "*) ;;
      *) echo '{}' > .mcp.json; echo travsr > CLAUDE.md; echo travsr > AGENTS.md ;;
    esac
    daemon
    echo "indexed"
    ;;
  repos)
    if [ -f "$d/remove_hangs" ]; then sleep 30; fi
    if [ -f "$d/remove_fails" ]; then echo "registry locked" >&2; exit 1; fi
    if grep -qx "$3" "$d/registry" 2>/dev/null; then echo "removed repo '$3'"; else echo "no repo named '$3' in the registry"; fi
    ;;
  mcp)
    daemon
    while IFS= read -r line; do
      id=$(printf '%s\n' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
      echo "req $line" >> "$d/requests"
      case "$line" in
        *'"method":"tools/list"'*)
          echo 'a log line, not JSON'
          echo '{"jsonrpc":"2.0","method":"notifications/message","params":{}}'
          printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"get_callers","inputSchema":{"type":"object","properties":{"symbol":{"type":"string"}},"required":["symbol"]}}]}}\n' "$id" ;;
        *'"name":"die"'*) exit 0 ;;
        *'"name":"hang"'*) sleep 30 ;;
        *'"name":"bad"'*)
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"unknown tool bad"}}\n' "$id" ;;
        *)
          esc=$(printf '%s' "$line" | sed 's/\\/\\\\/g; s/"/\\"/g')
          printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$esc"
          case "$line" in *'"name":"quit"'*) exit 0 ;; esac ;;
      esac
    done
    ;;
esac
exit 0
"#;

        /// Writes the script into `dir` and returns its path, once it can be run.
        pub(crate) fn travsr(dir: &Path) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("travsr");
            let text = SCRIPT.replace("@DIR@", &dir.display().to_string());
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            // Another test's fork may hold the file open for writing a moment ("text file
            // busy"); wait until it can be run.
            for _ in 0..200 {
                match std::process::Command::new(&path)
                    .arg("--version")
                    .current_dir(dir)
                    .status()
                {
                    Err(e) if e.raw_os_error() == Some(26) => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    _ => break,
                }
            }
            // That call is not one of the test's.
            let _ = std::fs::remove_file(dir.join("log"));
            let _ = std::fs::remove_file(dir.join("env.1"));
            path
        }

        /// One call of the fake: its arguments, working directory and environment.
        #[derive(Debug, Clone)]
        pub(crate) struct Call {
            pub(crate) argv: String,
            pub(crate) cwd: String,
            pub(crate) env: Vec<(String, String)>,
        }

        impl Call {
            pub(crate) fn var(&self, name: &str) -> Option<&str> {
                self.env
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.as_str())
            }
        }

        /// Every call so far, in order.
        pub(crate) fn calls(dir: &Path) -> Vec<Call> {
            let log = std::fs::read_to_string(dir.join("log")).unwrap_or_default();
            let lines: Vec<&str> = log.lines().collect();
            lines
                .chunks(2)
                .enumerate()
                .map(|(i, pair)| {
                    let env = std::fs::read_to_string(dir.join(format!("env.{}", i + 1)))
                        .unwrap_or_default()
                        .lines()
                        .filter_map(|l| l.split_once('='))
                        .map(|(n, v)| (n.to_string(), v.to_string()))
                        .collect();
                    Call {
                        argv: pair[0].trim_start_matches("argv ").to_string(),
                        cwd: pair
                            .get(1)
                            .map_or("", |c| c.trim_start_matches("cwd "))
                            .to_string(),
                        env,
                    }
                })
                .collect()
        }

        /// Calls whose arguments start with `prefix`.
        pub(crate) fn calls_of(dir: &Path, prefix: &str) -> Vec<Call> {
            calls(dir)
                .into_iter()
                .filter(|c| c.argv.starts_with(prefix))
                .collect()
        }

        /// The lines of `<dir>/<name>`, none when it does not exist.
        pub(crate) fn lines(dir: &Path, name: &str) -> Vec<String> {
            std::fs::read_to_string(dir.join(name))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    #[cfg(unix)]
    mod with_fake {
        use super::fake::{Call, calls, calls_of, lines, travsr};
        use super::*;
        use kestrel_sandbox::ProcessRunner;
        use std::process::Command as Std;

        /// Runs git in `dir` and returns its trimmed stdout.
        fn git(dir: &Path, args: &[&str]) -> String {
            let out = Std::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn commit(repo: &Path, text: &str) -> String {
            std::fs::write(repo.join("lib.rs"), text).unwrap();
            git(repo, &["add", "lib.rs"]);
            git(
                repo,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "commit",
                    "-q",
                    "-m",
                    "c",
                ],
            );
            git(repo, &["rev-parse", "HEAD"])
        }

        struct Setup {
            dir: tempfile::TempDir,
            /// The source repo; its first commit, and a later one.
            repo: PathBuf,
            first: String,
            /// A run's workspace, cloned from the source, which nothing here may touch.
            workspace: PathBuf,
            p: TravsrProcesses,
        }

        impl Setup {
            fn real(&self, path: &Path) -> String {
                std::fs::canonicalize(path).unwrap().display().to_string()
            }

            fn fake(&self) -> &Path {
                self.dir.path()
            }
        }

        /// A source repo with two commits, a workspace cloned from it, and processes on the fake
        /// travsr with real git, under `<dir>/data/travsr`.
        fn setup() -> Setup {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            let first = commit(&repo, "pub fn first() {}\n");
            let workspace = dir.path().join("workspace");
            git(
                dir.path(),
                &[
                    "clone",
                    "-q",
                    repo.to_str().unwrap(),
                    workspace.to_str().unwrap(),
                ],
            );
            commit(&repo, "pub fn second() {}\n");
            let mut p = TravsrProcesses::new(
                travsr(dir.path()),
                dir.path().join("data").join("travsr"),
                Arc::new(ProcessRunner),
                Duration::from_secs(30),
            );
            p.command_timeout = Duration::from_secs(10);
            Setup {
                dir,
                repo,
                first,
                workspace,
                p,
            }
        }

        /// Every file under `dir` with its contents, to see that nothing changed there.
        fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
            let mut out = Vec::new();
            let mut todo = vec![dir.to_path_buf()];
            while let Some(d) = todo.pop() {
                for entry in std::fs::read_dir(&d).unwrap() {
                    let path = entry.unwrap().path();
                    let meta = std::fs::symlink_metadata(&path).unwrap();
                    if meta.is_dir() {
                        todo.push(path.clone());
                        out.push((path, Vec::new()));
                    } else {
                        out.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
                    }
                }
            }
            out.sort();
            out
        }

        /// Kestrel's flags and environment, on every call: no daemon, no download, no registry,
        /// and nothing else from Kestrel's environment.
        fn assert_safe_env(call: &Call) {
            assert_eq!(call.var("CI"), Some("1"), "{call:?}");
            assert_eq!(call.var("TRAVSR_SKIP_DOWNLOAD"), Some("1"), "{call:?}");
            assert_eq!(call.var("TRAVSR_DISABLE_REGISTRY"), Some("1"), "{call:?}");
            for (name, _) in &call.env {
                let fine = PASSED_ENV.contains(&name.as_str())
                    || SET_ENV.iter().any(|(n, _)| n == name)
                    || ["PWD", "SHLVL", "_", "OLDPWD"].contains(&name.as_str());
                assert!(fine, "{name} reached travsr");
            }
        }

        #[test]
        fn the_fake_does_what_real_travsr_does_without_kestrel_s_flags() {
            // So the tests below, which see none of it, are not empty.
            let s = setup();
            let out = Std::new(&s.p.command)
                .arg("init")
                .current_dir(&s.workspace)
                .env_remove("CI")
                .env_remove("TRAVSR_SKIP_DOWNLOAD")
                .env_remove("TRAVSR_DISABLE_REGISTRY")
                .output()
                .unwrap();
            assert!(out.status.success());
            for file in [".mcp.json", "CLAUDE.md", "AGENTS.md", ".travsr/graph.db"] {
                assert!(s.workspace.join(file).exists(), "{file}");
            }
            let at = s.real(&s.workspace);
            assert_eq!(lines(s.fake(), "daemons"), [format!("daemon in {at}")]);
            assert_eq!(lines(s.fake(), "downloads"), [format!("download in {at}")]);
            assert_eq!(lines(s.fake(), "registry"), [at.clone()]);
            // Outside a git repo, init fails; an unknown repo is no error to remove.
            let out = Std::new(&s.p.command)
                .arg("init")
                .current_dir(s.fake())
                .output()
                .unwrap();
            assert!(!out.status.success());
            let out = Std::new(&s.p.command)
                .args(["repos", "--remove", "/nowhere"])
                .output()
                .unwrap();
            assert!(out.status.success());
            assert!(String::from_utf8_lossy(&out.stdout).contains("no repo named"));
        }

        #[tokio::test]
        async fn s1_index_copies_the_source_at_the_run_s_commit_and_never_touches_the_workspace() {
            let s = setup();
            let before = snapshot(&s.workspace);
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            // The copy is the source at the run's commit, not the source's head.
            let copy = s.p.copy(run);
            assert_eq!(git(&copy, &["rev-parse", "HEAD"]), s.first);
            assert_eq!(
                std::fs::read_to_string(copy.join("lib.rs")).unwrap(),
                "pub fn first() {}\n"
            );
            // One `init --no-connect`, in the copy, with Kestrel's environment.
            let inits = calls_of(s.fake(), "init");
            assert_eq!(inits.len(), 1);
            assert_eq!(inits[0].argv, "init --no-connect");
            assert_eq!(inits[0].cwd, s.real(&copy));
            assert_safe_env(&inits[0]);
            // So no daemon, no download, no registry entry, and no connect file.
            for name in ["daemons", "downloads", "registry"] {
                assert!(lines(s.fake(), name).is_empty(), "{name}");
            }
            for file in [".mcp.json", "CLAUDE.md", "AGENTS.md"] {
                assert!(!copy.join(file).exists(), "{file}");
            }
            assert!(copy.join(".travsr/graph.db").is_file());
            // The workspace is exactly as it was.
            assert_eq!(snapshot(&s.workspace), before);
        }

        #[tokio::test]
        async fn s2_a_failed_init_says_why_with_the_tail_of_its_output() {
            let s = setup();
            std::fs::write(s.fake().join("init_fails"), "").unwrap();
            let run = RunId::new();
            let err =
                s.p.index(run, &at(&s.repo, Some(&s.first)))
                    .await
                    .unwrap_err();
            assert!(err.contains("`travsr init` exited with code 3"), "{err}");
            assert!(err.contains("boom: no space left"), "{err}");
            assert!(!err.contains("indexing 3 files"), "{err}");
            let err = s.p.request(run, "tools/list", json!({})).await.unwrap_err();
            assert!(err.contains("not available"), "{err}");
        }

        #[tokio::test]
        async fn s2_a_dropped_index_kills_init() {
            let s = setup();
            std::fs::write(s.fake().join("init_sleeps"), "").unwrap();
            let run = RunId::new();
            let source = at(&s.repo, Some(&s.first));
            let gave_up =
                tokio::time::timeout(Duration::from_millis(500), s.p.index(run, &source)).await;
            assert!(gave_up.is_err());
            // Killed: it never gets to make the index.
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!s.p.copy(run).join(".travsr").exists());
            assert!(!s.p.dir(run).join("indexed").exists());
        }

        #[tokio::test]
        async fn a_resumed_run_reuses_its_copy_and_remakes_a_lost_one_at_its_commit() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            // Resumed: no commit given, the copy is reused as it is.
            s.p.index(run, &at(&s.repo, None)).await.unwrap();
            assert_eq!(calls_of(s.fake(), "init").len(), 1);
            s.p.request(run, "tools/list", json!({})).await.unwrap();
            // The copy is lost: made again at the recorded commit, though the source moved on.
            std::fs::remove_dir_all(s.p.copy(run)).unwrap();
            s.p.index(run, &at(&s.repo, None)).await.unwrap();
            assert_eq!(calls_of(s.fake(), "init").len(), 2);
            assert_eq!(git(&s.p.copy(run), &["rev-parse", "HEAD"]), s.first);
            // A copy whose index went missing is indexed again.
            std::fs::remove_file(s.p.copy(run).join(".travsr/graph.db")).unwrap();
            s.p.index(run, &at(&s.repo, None)).await.unwrap();
            assert_eq!(calls_of(s.fake(), "init").len(), 3);
        }

        #[tokio::test]
        async fn s3_calls_go_to_one_server_per_run_bound_to_its_own_copy() {
            let s = setup();
            let (a, b) = (RunId::new(), RunId::new());
            for run in [a, b] {
                s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            }
            let listed = s.p.request(a, "tools/list", json!({})).await.unwrap();
            assert_eq!(listed["tools"][0]["name"], "get_callers");
            let called =
                s.p.request(
                    a,
                    "tools/call",
                    json!({ "name": "get_callers", "arguments": { "symbol": "x\"y\\z" } }),
                )
                .await
                .unwrap();
            let text = called["content"][0]["text"].as_str().unwrap();
            let sent: Value = serde_json::from_str(text).unwrap();
            assert_eq!(sent["params"]["arguments"]["symbol"], "x\"y\\z");
            s.p.request(b, "tools/call", json!({ "name": "get_callers" }))
                .await
                .unwrap();
            // One server per run, each on its own copy's index, in its copy, with no daemon.
            let servers = calls_of(s.fake(), "mcp");
            assert_eq!(servers.len(), 2);
            for (server, run) in servers.iter().zip([a, b]) {
                let db = s.p.copy(run).join(".travsr").join("graph.db");
                assert_eq!(server.argv, format!("mcp --stdio --db {}", db.display()));
                assert_eq!(server.cwd, s.real(&s.p.copy(run)));
                assert_safe_env(server);
            }
            assert!(lines(s.fake(), "daemons").is_empty());
        }

        #[tokio::test]
        async fn an_index_swapped_for_a_link_is_never_served() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            let db = s.p.copy(run).join(".travsr/graph.db");
            let other = s.fake().join("other.db");
            std::fs::write(&other, "").unwrap();
            std::fs::remove_file(&db).unwrap();
            std::os::unix::fs::symlink(&other, &db).unwrap();
            let err = s.p.request(run, "tools/list", json!({})).await.unwrap_err();
            assert!(err.contains("not a regular file"), "{err}");
            assert!(calls_of(s.fake(), "mcp").is_empty());
        }

        #[tokio::test]
        async fn s8_forget_stops_the_server_unregisters_the_copy_and_deletes_it() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            s.p.request(run, "tools/list", json!({})).await.unwrap();
            assert!(lock(&s.p.served).servers.contains_key(&run));
            let before = snapshot(&s.workspace);
            s.p.forget(run).await.unwrap();
            assert!(!lock(&s.p.served).servers.contains_key(&run));
            let removes = calls_of(s.fake(), "repos");
            let copy = std::fs::canonicalize(&s.p.root)
                .unwrap()
                .join(run.to_string())
                .join("repo");
            assert_eq!(removes.len(), 1);
            assert_eq!(
                removes[0].argv,
                format!("repos --remove {}", copy.display())
            );
            assert_safe_env(&removes[0]);
            assert!(!s.p.dir(run).exists());
            assert_eq!(snapshot(&s.workspace), before);
            // Not served any more, and forgetting again is fine (Travsr exits 0 for a path it
            // does not know).
            let err = s.p.request(run, "tools/list", json!({})).await.unwrap_err();
            assert!(err.contains("not available"), "{err}");
            s.p.forget(run).await.unwrap();
        }

        #[tokio::test]
        async fn s8_a_failed_or_hung_remove_is_an_error_and_the_copy_is_still_deleted() {
            let mut s = setup();
            s.p.command_timeout = Duration::from_secs(2);
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            std::fs::write(s.fake().join("remove_fails"), "").unwrap();
            let err = s.p.forget(run).await.unwrap_err();
            assert!(
                err.contains("repos --remove") && err.contains("registry locked"),
                "{err}"
            );
            assert!(!s.p.dir(run).exists());
            std::fs::remove_file(s.fake().join("remove_fails")).unwrap();
            std::fs::write(s.fake().join("remove_hangs"), "").unwrap();
            let started = std::time::Instant::now();
            let err = s.p.forget(run).await.unwrap_err();
            assert!(err.contains("did not finish within 2 s"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(10));
        }

        #[tokio::test]
        async fn release_stops_the_server_and_keeps_the_copy() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            s.p.request(run, "tools/list", json!({})).await.unwrap();
            s.p.release(run).await;
            assert!(lock(&s.p.served).servers.is_empty());
            assert!(s.p.copy(run).join(".travsr/graph.db").is_file());
            assert!(s.p.request(run, "tools/list", json!({})).await.is_err());
            assert!(calls_of(s.fake(), "repos").is_empty());
            assert_eq!(s.p.kept().await.unwrap(), [run]);
        }

        #[tokio::test]
        async fn s7_a_server_that_dies_is_an_error_and_is_started_again() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            let err =
                s.p.request(run, "tools/call", json!({ "name": "die" }))
                    .await
                    .unwrap_err();
            assert!(err.contains("stopped"), "{err}");
            // Answers, then exits: the next call finds it dead and starts another.
            s.p.request(run, "tools/call", json!({ "name": "quit" }))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            s.p.request(run, "tools/call", json!({ "name": "get_callers" }))
                .await
                .unwrap();
            assert_eq!(calls_of(s.fake(), "mcp").len(), 3);
        }

        #[tokio::test]
        async fn s7_travsr_s_error_is_passed_on_and_the_server_kept() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            let err =
                s.p.request(run, "tools/call", json!({ "name": "bad" }))
                    .await
                    .unwrap_err();
            assert_eq!(err, "unknown tool bad");
            s.p.request(run, "tools/call", json!({ "name": "get_callers" }))
                .await
                .unwrap();
            assert_eq!(calls_of(s.fake(), "mcp").len(), 1);
        }

        #[tokio::test]
        async fn s7_a_call_given_up_on_leaves_no_stale_answer_behind() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            let gave_up = tokio::time::timeout(
                Duration::from_millis(300),
                s.p.request(run, "tools/call", json!({ "name": "hang" })),
            )
            .await;
            assert!(gave_up.is_err());
            // The server that may still answer the old call is replaced.
            let answer =
                s.p.request(run, "tools/call", json!({ "name": "get_callers" }))
                    .await
                    .unwrap();
            assert!(
                answer["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("get_callers")
            );
            assert_eq!(calls_of(s.fake(), "mcp").len(), 2);
        }

        #[tokio::test]
        async fn s7_a_call_that_hangs_times_out_inside_and_never_holds_the_run() {
            let mut s = setup();
            s.p.call_timeout = Duration::from_secs(1);
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            let err =
                s.p.request(run, "tools/call", json!({ "name": "hang" }))
                    .await
                    .unwrap_err();
            assert!(err.contains("did not answer within 1 s"), "{err}");
            // The next call gets a new server at once.
            s.p.request(run, "tools/call", json!({ "name": "get_callers" }))
                .await
                .unwrap();
            assert_eq!(calls_of(s.fake(), "mcp").len(), 2);
        }

        #[tokio::test]
        async fn forget_does_not_wait_long_for_a_call_that_holds_the_server() {
            // The call may take 30 s; forget waits for it at most 2 s.
            let mut s = setup();
            s.p.command_timeout = Duration::from_secs(2);
            let s = Arc::new(s);
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            let call = tokio::spawn({
                let s = s.clone();
                async move {
                    s.p.request(run, "tools/call", json!({ "name": "hang" }))
                        .await
                }
            });
            while lines(s.fake(), "requests").is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let started = std::time::Instant::now();
            s.p.forget(run).await.unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{:?}",
                started.elapsed()
            );
            assert!(!s.p.dir(run).exists());
            // Nothing starts the run's server again, the call still under way included.
            call.abort();
            let err = s.p.request(run, "tools/list", json!({})).await.unwrap_err();
            assert!(err.contains("not available"), "{err}");
        }

        /// A sandbox whose agent runs until it is stopped.
        struct Idle;

        #[async_trait]
        impl kestrel_sandbox::SandboxDriver for Idle {
            fn name(&self) -> &'static str {
                "idle"
            }
            fn is_strong(&self) -> bool {
                true
            }
            async fn create(
                &self,
                _: &kestrel_sandbox::LaunchSpec,
            ) -> kestrel_sandbox::Result<kestrel_sandbox::SandboxHandle> {
                Ok(kestrel_sandbox::SandboxHandle { id: "idle".into() })
            }
            async fn start(
                &self,
                _: &kestrel_sandbox::SandboxHandle,
            ) -> kestrel_sandbox::Result<kestrel_sandbox::OutputStream> {
                Ok(Box::pin(futures::stream::pending()))
            }
            async fn wait(
                &self,
                _: &kestrel_sandbox::SandboxHandle,
            ) -> kestrel_sandbox::Result<kestrel_sandbox::ExitStatus> {
                Ok(kestrel_sandbox::ExitStatus { code: 0 })
            }
            async fn stop(
                &self,
                _: &kestrel_sandbox::SandboxHandle,
            ) -> kestrel_sandbox::Result<()> {
                Ok(())
            }
            async fn destroy(
                &self,
                _: &kestrel_sandbox::SandboxHandle,
            ) -> kestrel_sandbox::Result<()> {
                Ok(())
            }
        }

        fn spec(repo: &Path, mcp: &str) -> kestrel_types::AgentSpec {
            let mut spec = kestrel_types::AgentSpec::from_yaml(&format!(
                "name: reader\nruntime: claude-code\nrole: r\ncapabilities: {{ mcp: {mcp} }}\n\
                 sandbox: {{ tier: auto, cpu: 1, memory: 512Mi, workspace: git-clone }}\n\
                 budget: {{ usd: \"1\", wall_clock: 1h }}\n"
            ))
            .unwrap();
            spec.sandbox.workspace = kestrel_types::WorkspaceSpec::GitClone {
                repo: repo.to_path_buf(),
                depth: None,
            };
            spec
        }

        /// Scenarios 1, 3, 5 and 9 end to end: a real engine with real git has the run's code
        /// copied and indexed with the fake travsr, and the gateway host reaches that run's
        /// server only. The run's workspace is never touched by Travsr.
        #[tokio::test]
        async fn the_gateway_host_reaches_the_run_s_own_private_copy() {
            let s = setup();
            let data = s.fake().join("data");
            let store = Arc::new(kestrel_store::Store::open_in_memory().unwrap());
            let processes = Arc::new(TravsrProcesses::new(
                s.p.command.clone(),
                data.join("travsr"),
                Arc::new(ProcessRunner),
                Duration::from_secs(5),
            ));
            let engine = Arc::new(
                Engine::new(
                    store.clone(),
                    vec![Box::new(Idle)],
                    Arc::new(kestrel_agent::adapter_for),
                    url::Url::parse("http://127.0.0.1:7070").unwrap(),
                    kestrel_core::Limits::default(),
                )
                .with_workspaces(kestrel_core::Workspaces::new(
                    data.join("workspaces"),
                    Arc::new(ProcessRunner),
                ))
                .with_indexer(processes.clone(), kestrel_core::IndexLimits::default()),
            );
            let host = EngineTravsr {
                engine: engine.clone(),
                processes,
                call_timeout: Duration::from_secs(7),
            };
            assert_eq!(host.call_timeout(), Duration::from_secs(7));
            assert_eq!(host.tools(RunId::new()).await, Err(ToolsError::Gone));
            let stop = tokio_util::sync::CancellationToken::new();
            let running = tokio::spawn({
                let (engine, stop) = (engine.clone(), stop.clone());
                async move { engine.run_forever(stop).await }
            });
            let run = engine
                .submit(spec(&s.repo, "[travsr.search_symbol]"), "read".into())
                .await
                .unwrap();
            let other = engine
                .submit(spec(&s.repo, "[kestrel.join]"), "other".into())
                .await
                .unwrap();
            for _ in 0..1500 {
                if engine.travsr_ready(run) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(engine.travsr_ready(run), "the run is indexed");
            let workspace = data.join("workspaces").join(run.to_string());
            let copy = data.join("travsr").join(run.to_string()).join("repo");
            // The copy is at the commit the workspace was cloned at.
            assert_eq!(
                git(&copy, &["rev-parse", "HEAD"]),
                git(&workspace, &["rev-parse", "HEAD"])
            );
            assert_eq!(host.tools(run).await, Ok(vec!["search_symbol".to_string()]));
            let schemas = host.schemas(run).await.unwrap();
            assert_eq!(schemas[0]["name"], "get_callers");
            let called = host
                .call(run, "search_symbol", json!({ "name": "Engine" }))
                .await
                .unwrap();
            let sent: Value =
                serde_json::from_str(called["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(sent["params"]["name"], "search_symbol");
            assert_eq!(sent["params"]["arguments"], json!({ "name": "Engine" }));
            // Every Travsr call ran in the run's copy; none in a workspace.
            let real_copy = s.real(&copy);
            for call in calls(s.fake()) {
                assert_eq!(call.cwd, real_copy, "{call:?}");
            }
            assert!(!workspace.join(".travsr").exists());
            // The other run has no Travsr: no tools, and no server to call.
            assert_eq!(host.tools(other).await, Ok(Vec::new()));
            let err = host
                .call(other, "search_symbol", json!({}))
                .await
                .unwrap_err();
            assert!(err.contains("not available"), "{err}");
            assert!(host.schemas(other).await.is_err());
            host.denied(run, "travsr.get_callers").await;
            let logged = store.events_page(run, 0, 1_000).unwrap().into_iter().any(|e| {
                matches!(e.kind, kestrel_types::EventKind::AgentLog { ref line } if line.contains("travsr.get_callers"))
            });
            assert!(logged);
            stop.cancel();
            running.await.unwrap().unwrap();
        }

        /// Scenario 12, reworked: the index answers from the run's private copy, and an edit the
        /// agent makes in its workspace never reaches the host's index. Against a real Travsr:
        /// `TRAVSR_BIN`, else `travsr` on `PATH`; skipped, saying so, when neither runs.
        /// `cargo test -p kestrel-cli --features integration`.
        #[cfg(feature = "integration")]
        #[tokio::test]
        async fn s12_the_index_answers_from_the_private_copy_and_never_sees_the_workspace() {
            let bin = std::env::var_os("TRAVSR_BIN")
                .map_or_else(|| PathBuf::from("travsr"), PathBuf::from);
            if Std::new(&bin).arg("--version").output().is_err() {
                eprintln!("SKIPPED: no travsr binary (set TRAVSR_BIN)");
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            let first = commit(&repo, "pub fn alpha_kestrel_marker() {}\n");
            let workspace = dir.path().join("workspace");
            git(
                dir.path(),
                &[
                    "clone",
                    "-q",
                    repo.to_str().unwrap(),
                    workspace.to_str().unwrap(),
                ],
            );
            // The agent edits its workspace before the index is made.
            std::fs::write(
                workspace.join("lib.rs"),
                "pub fn alpha_kestrel_marker() {}\npub fn beta_kestrel_marker() {}\n",
            )
            .unwrap();
            let before = snapshot(&workspace);
            let p = TravsrProcesses::new(
                bin,
                dir.path().join("data").join("travsr"),
                Arc::new(ProcessRunner),
                Duration::from_secs(60),
            );
            let run = RunId::new();
            let find =
                |name: &str| json!({ "name": "search_symbol", "arguments": { "name": name } });
            p.index(run, &at(&repo, Some(&first))).await.unwrap();
            let found = p
                .request(run, "tools/call", find("alpha_kestrel_marker"))
                .await
                .unwrap();
            assert!(
                found.to_string().contains("alpha_kestrel_marker"),
                "{found}"
            );
            let found = p
                .request(run, "tools/call", find("beta_kestrel_marker"))
                .await
                .unwrap();
            assert!(
                !found.to_string().contains("beta_kestrel_marker"),
                "{found}"
            );
            // Real Travsr wrote nothing into the workspace, and nothing outside the copy.
            assert_eq!(snapshot(&workspace), before);
            for file in [".mcp.json", "CLAUDE.md", "AGENTS.md"] {
                assert!(!p.copy(run).join(file).exists(), "{file}");
            }
            p.forget(run).await.unwrap();
            assert!(!p.dir(run).exists());
        }
    }
}
