//! Travsr for `serve` (#44), when `kestrel.toml` has a `[travsr]` section.
//!
//! Travsr runs on the host, as Kestrel's user, so it never touches a run's workspace: the agent
//! may rewrite anything there, and Travsr would read it, run git in it and serve files by the
//! paths its index names. Instead:
//!
//! * [`TravsrProcesses`] keeps a private copy of each indexed run's code at
//!   `<data dir>/travsr/<run id>/repo`: a clone of the run's source repo at the commit its
//!   workspace was cloned at (`kestrel_core::workspace::clone_at`). The `travsr` directory and
//!   each run's directory are owner-only (0700), and no sandbox ever mounts them. A link the
//!   commit holds is a plain file in the copy, so nothing Travsr writes there (`.gitignore`,
//!   `.travsrignore`, its index) can go through one to the host.
//! * It indexes that copy with `travsr init --no-connect`, with `CI=1` (no background daemon),
//!   `TRAVSR_SKIP_DOWNLOAD=1` (no tool or model download) and `TRAVSR_DISABLE_REGISTRY=1` (no
//!   entry in Travsr's global registry), and records the commit, so a resumed run reuses or
//!   remakes the same copy. A `.travsr` the commit holds is deleted first (one that is a link is
//!   refused, as defence in depth). On Unix `init` runs in its own process group, killed whole
//!   when Kestrel gives up.
//! * Each run that calls a tool gets its own `travsr mcp --stdio --db <copy>/.travsr/graph.db`,
//!   started on its first call in the copy, once the database is checked to be a regular file
//!   inside the run's directory.
//! * Forgetting a run stops its server and deletes the run's directory. No Travsr command runs:
//!   nothing was registered.
//! * [`EngineTravsr`] is what the gateway asks: which tools a run may call (from the engine) and
//!   the calls themselves, always to the run's own server.
//!
//! Travsr gets a short list of environment variables (see [`PASSED_ENV`] and [`SET_ENV`]), never
//! the run token or a provider key. The command is an absolute path (see [`resolve_command`]),
//! so nothing in the copy, which is Travsr's working directory, can stand in for it.

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
    /// The travsr command: an absolute path, or a bare name looked up on `PATH` once, when
    /// `serve` starts (see [`resolve_command`]).
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
/// * `TRAVSR_DISABLE_REGISTRY`: the copy is not added to Travsr's global registry. Travsr checks
///   it at both places `init` writes the registry (`travsr_store::registry::register` and
///   `mark_indexed`, in `crates/travsr-daemon/src/lib.rs` of the travsr repo), so forgetting a
///   run has nothing to unregister and runs no Travsr command.
const SET_ENV: &[(&str, &str)] = &[
    ("CI", "1"),
    ("TRAVSR_SKIP_DOWNLOAD", "1"),
    ("TRAVSR_DISABLE_REGISTRY", "1"),
];

/// Most bytes of a failed command's output kept for its error.
const TAIL: usize = 500;
/// Most bytes of each of `init`'s output streams held in memory while it runs: the latest ones.
const KEPT_OUTPUT: usize = 4096;
/// Longest line read from a Travsr server; a longer answer is an error.
const MAX_LINE: u64 = 16 * 1024 * 1024;
/// The longest a wait for a call to give a run's server up may take.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// What the agent is told when its run's server cannot be started. The details (host paths
/// among them) go to Kestrel's log only.
const NOT_STARTED: &str =
    "Travsr's server for this run could not be started; Kestrel's log on the host says why";

/// The absolute entries of `path`, in order: Travsr runs with the copy as its working directory,
/// so a relative entry (`.`, an empty one, `bin`) could run a file committed in the source.
/// `None` when no entry is absolute.
fn absolute_path_entries(path: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let kept: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|p| p.is_absolute())
        .collect();
    if kept.is_empty() {
        return None;
    }
    std::env::join_paths(kept).ok()
}

/// The environment of every Travsr command, from Kestrel's own (`vars`): only the variables
/// [`PASSED_ENV`] names, `PATH` cut to its absolute entries (left out when none is), then
/// [`SET_ENV`] on top.
fn travsr_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let is_path = |name: &str| {
        if cfg!(windows) {
            name.eq_ignore_ascii_case("PATH")
        } else {
            name == "PATH"
        }
    };
    let mut env: Vec<_> = vars
        .into_iter()
        .filter_map(|(name, value)| {
            let text = name.to_str()?;
            if !passed(text) {
                return None;
            }
            if is_path(text) {
                return absolute_path_entries(&value).map(|kept| (name, kept));
            }
            Some((name, value))
        })
        .collect();
    env.extend(SET_ENV.iter().map(|(n, v)| ((*n).into(), (*v).into())));
    env
}

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

/// Whether `command` may be the travsr command: an absolute path, or a bare name (one plain
/// component) to look up on `PATH`. A relative path such as `./travsr` or `bin/travsr` is not:
/// Travsr runs with the run's copy as its working directory, where it would resolve. Nor, on
/// Windows, is `\travsr`, which has a root but no drive.
pub(crate) fn acceptable_command(command: &Path) -> bool {
    use std::path::Component;
    if command.is_absolute() {
        return true;
    }
    let mut parts = command.components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// The travsr command to run, as an absolute path: `command` itself when it is one, else the
/// first runnable file of that name in an absolute entry of `path` (Kestrel's `PATH`). A
/// relative entry is skipped, as the copy would resolve it. Done once, when `serve` starts.
/// The `Err` names neither the command nor `PATH`.
pub(crate) fn resolve_command(command: &Path, path: Option<&OsStr>) -> Result<PathBuf, String> {
    if command.is_absolute() {
        return Ok(command.to_path_buf());
    }
    if !acceptable_command(command) {
        return Err("travsr.command is neither an absolute path nor a bare command name".into());
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| candidates(&dir, command))
        .find(|candidate| runnable(candidate))
        .ok_or_else(|| "travsr.command was not found in any absolute PATH entry".to_string())
}

/// The files `name` may be in `dir`: on Windows `name.exe` first, when `name` has no extension.
fn candidates(dir: &Path, name: &Path) -> Vec<PathBuf> {
    let plain = dir.join(name);
    if cfg!(windows) && name.extension().is_none() {
        vec![plain.with_extension("exe"), plain]
    } else {
        vec![plain]
    }
}

/// Whether `path` is a file that can be run (on Unix, with an execute bit).
fn runnable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// The process group of a command started as the leader of its own group (on Unix). Dropped
/// while armed (Kestrel gave up on the command, or could not wait for it), it kills the whole
/// group, so helpers the command started go with it. Elsewhere it holds nothing and does
/// nothing.
///
/// A group's id is a process id, so it may be reused once the leader is reaped and no process
/// is left in the group. While Kestrel waits, the leader is not reaped, so killing the group
/// then cannot hit another one. Once the leader ended by itself (and was reaped) and its output
/// reached its end, the group is disarmed and left alone. What remains:
/// * a helper that let go of the output and kept running is not killed after a clean exit;
/// * if the leader was reaped but its output did not reach its end (a read error, or Kestrel
///   gave up while a helper held it), the group is still killed: a helper holding the output
///   keeps the id in use, but after a read error the group may be empty and its id reused.
struct Group(Option<u32>);

impl Group {
    fn of(child: &Child) -> Group {
        Group(if cfg!(unix) { child.id() } else { None })
    }

    /// The command ended by itself and its output reached its end: its group is left alone.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(group) = self.0.take() {
            kill_group(group);
        }
    }
}

/// Sends `SIGKILL` to process group `group`. A group with nothing left in it is fine; any other
/// failure is logged. Groups 0 and 1 are never a command's, and `-1` would signal every process,
/// so they are never signalled.
#[cfg(unix)]
fn kill_group(group: u32) {
    use rustix::process::{Pid, Signal, kill_process_group};
    let Some(pid) = i32::try_from(group)
        .ok()
        .filter(|g| *g > 1)
        .and_then(Pid::from_raw)
    else {
        return;
    };
    if let Err(e) = kill_process_group(pid, Signal::KILL)
        && e != rustix::io::Errno::SRCH
    {
        eprintln!("kestrel: could not kill the process group {group} of `travsr init`: {e}");
    }
}

/// Process groups are Unix only; [`Group::of`] holds none elsewhere.
#[cfg(not(unix))]
fn kill_group(_: u32) {}

/// Reads `reader` to its end and returns the last [`KEPT_OUTPUT`] bytes it gave, never holding
/// much more, and whether it reached the end. A read error ends it early. No reader has ended.
async fn tail_of<R: tokio::io::AsyncRead + Unpin>(reader: Option<R>) -> (Vec<u8>, bool) {
    let mut tail = Vec::new();
    let Some(mut reader) = reader else {
        return (tail, true);
    };
    let mut buf = [0u8; 8192];
    loop {
        let Ok(read) = reader.read(&mut buf).await else {
            return (tail, false);
        };
        if read == 0 {
            return (tail, true);
        }
        tail.extend_from_slice(&buf[..read]);
        if tail.len() > KEPT_OUTPUT {
            let over = tail.len() - KEPT_OUTPUT;
            tail.drain(..over);
        }
    }
}

/// The last [`TAIL`] bytes of `said`, trimmed, on a character boundary.
fn last_words(said: &[u8]) -> String {
    let text = String::from_utf8_lossy(said);
    let text = text.trim();
    let mut start = text.len().saturating_sub(TAIL);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// Deletes the `.travsr` the copy's commit holds, if any, before `init` runs: Travsr writes its
/// index to `<repo>/.travsr/graph.db` and would use a committed one as it is.
///
/// A `.travsr` that is a link could send those writes anywhere, so it is refused. The copy never
/// holds one (`clone_at` checks out a committed link as a plain file, which is deleted here), so
/// this refusal is defence in depth, kept in case the copy is ever made another way.
fn clear_committed_index(copy: &Path) -> Result<(), String> {
    let index = copy.join(".travsr");
    let cleared = match std::fs::symlink_metadata(&index) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(
                "the run's commit holds a `.travsr` that is a link, so Kestrel does not index it"
                    .into(),
            );
        }
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&index),
        Ok(_) => std::fs::remove_file(&index),
    };
    cleared.map_err(|e| format!("could not delete the `.travsr` the run's commit holds: {e}"))
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
    /// The longest a wait for a call to give a server up may take.
    stop_timeout: Duration,
    served: Mutex<Served>,
}

impl TravsrProcesses {
    /// `command` is run only when it is an absolute path (see [`resolve_command`]); any other
    /// makes every index fail, saying so.
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
            stop_timeout: STOP_TIMEOUT,
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
    /// Refused unless the command is an absolute path.
    fn command<S: AsRef<OsStr>>(
        &self,
        args: impl IntoIterator<Item = S>,
    ) -> Result<Command, String> {
        if !self.command.is_absolute() {
            return Err(
                "travsr.command was not found on PATH when `kestrel serve` started".to_string(),
            );
        }
        let mut cmd = Command::new(&self.command);
        cmd.args(args)
            .env_clear()
            .envs(travsr_env(std::env::vars_os()))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        Ok(cmd)
    }

    /// Runs `travsr init --no-connect` in `copy` to the end. On Unix it leads a process group of
    /// its own, killed whole when Kestrel gives up on it (a drop included) or cannot wait for
    /// it, and left alone when it ends by itself (see [`Group`]). Only the latest output is held
    /// in memory; `Err` ends with the tail of it.
    async fn init(&self, copy: &Path) -> Result<(), String> {
        let mut cmd = self.command(["init", "--no-connect"])?;
        cmd.current_dir(copy)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", self.command.display()))?;
        // Declared after `child`, so dropped first: the group is killed before the leader is.
        let mut group = Group::of(&child);
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let ((out, out_ended), (err, err_ended), status) =
            tokio::join!(tail_of(stdout), tail_of(stderr), child.wait());
        let status = status.map_err(|e| format!("`travsr init` could not be waited for: {e}"))?;
        if out_ended && err_ended {
            group.disarm();
        }
        if status.success() {
            return Ok(());
        }
        let said = if err.iter().all(u8::is_ascii_whitespace) {
            &out
        } else {
            &err
        };
        let code = status
            .code()
            .map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
        Err(format!(
            "`travsr init` exited with {code}: {}",
            last_words(said)
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
    /// keeps the server at most `stop_timeout`; past that, the server goes when that call ends
    /// (at most its own timeout later).
    async fn stop(&self, run: RunId) {
        let slot = {
            let mut served = lock(&self.served);
            served.runs.remove(&run);
            served.servers.remove(&run)
        };
        if let Some(slot) = slot
            && let Ok(mut held) = tokio::time::timeout(self.stop_timeout, slot.lock()).await
        {
            held.closed = true;
            held.server.take();
        }
    }

    /// Asks `run`'s server, starting it if needed, at most `call_timeout`. A server that died,
    /// that a dropped call left halfway, or that did not answer in time, is started again on the
    /// next call. A server that cannot be started is logged on the host with the reason (which
    /// may name host paths), and the `Err` the agent gets says only that.
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
            None => match self.start(run) {
                Ok(server) => held.server.insert(server),
                Err(why) => {
                    eprintln!("kestrel: Travsr's server for run {run} could not be started: {why}");
                    return Err(NOT_STARTED.into());
                }
            },
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
            .command([OsStr::new("mcp"), OsStr::new("--stdio"), OsStr::new("--db")])?
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
        clear_committed_index(&copy)?;
        self.init(&copy).await?;
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

    /// Stops `run`'s server and deletes its directory; a run with none is forgotten already. No
    /// Travsr command runs: with `TRAVSR_DISABLE_REGISTRY=1` (see [`SET_ENV`]) `init` registered
    /// nothing, so there is nothing to unregister.
    async fn forget(&self, run: RunId) -> Result<(), String> {
        self.stop(run).await;
        remove_dir_if_any(&self.dir(run))
            .await
            .map_err(|e| format!("could not delete {}: {e}", self.dir(run).display()))
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

    // Travsr's own children (git among them) are looked up on the PATH it gets, with the copy as
    // working directory, so a relative entry could run a file committed in the source.
    #[test]
    fn travsr_gets_only_the_absolute_path_entries() {
        let abs = std::env::temp_dir();
        let other = abs.join("bin");
        let path = std::env::join_paths([
            abs.clone(),
            PathBuf::from("."),
            PathBuf::from(""),
            PathBuf::from("rel/dir"),
            other.clone(),
        ])
        .unwrap();
        let kept: Vec<PathBuf> =
            std::env::split_paths(&absolute_path_entries(&path).unwrap()).collect();
        assert_eq!(kept, [abs, other]);
        let none = std::env::join_paths([PathBuf::from("."), PathBuf::from("rel")]).unwrap();
        assert_eq!(absolute_path_entries(&none), None);
    }

    // The environment every Travsr command gets, from a given one: locations kept, PATH cut to
    // its absolute entries, secrets dropped, Kestrel's own settings added.
    #[test]
    fn travsr_env_keeps_locations_cuts_path_and_adds_kestrel_s_settings() {
        use std::collections::BTreeMap;
        use std::ffi::OsString;
        let abs = std::env::temp_dir();
        let path = std::env::join_paths([PathBuf::from("."), abs.clone()]).unwrap();
        let given = [
            (OsString::from("PATH"), path),
            (OsString::from("HOME"), OsString::from("/home/op")),
            (OsString::from("ANTHROPIC_API_KEY"), OsString::from("sk")),
            (OsString::from("CI"), OsString::from("0")),
        ];
        let env: BTreeMap<OsString, OsString> = travsr_env(given).into_iter().collect();
        let kept: Vec<PathBuf> = std::env::split_paths(&env[&OsString::from("PATH")]).collect();
        assert_eq!(kept, [abs]);
        assert_eq!(env[&OsString::from("HOME")], OsString::from("/home/op"));
        assert!(!env.contains_key(&OsString::from("ANTHROPIC_API_KEY")));
        assert_eq!(env[&OsString::from("CI")], OsString::from("1"));
        assert_eq!(
            env[&OsString::from("TRAVSR_DISABLE_REGISTRY")],
            OsString::from("1")
        );
        // A PATH with no absolute entry is not passed at all.
        let only_relative = [(OsString::from("PATH"), OsString::from("."))];
        assert!(
            !travsr_env(only_relative)
                .iter()
                .any(|(n, _)| n == &OsString::from("PATH"))
        );
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
        // Forgetting needs no Travsr: it deletes the copy.
        assert!(p.dir(run).exists());
        p.forget(run).await.unwrap();
        assert!(!p.dir(run).exists());
        // A run with no directory at all is forgotten already.
        p.forget(RunId::new()).await.unwrap();
    }

    #[tokio::test]
    async fn a_command_that_is_not_absolute_is_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let p = TravsrProcesses::new(
            PathBuf::from("travsr"),
            dir.path().join("travsr"),
            Arc::new(FakeGit),
            Duration::from_secs(5),
        );
        let run = RunId::new();
        let err = p.index(run, &at(dir.path(), Some(SHA))).await.unwrap_err();
        assert!(err.contains("not found on PATH"), "{err}");
        // Nor as a server, with an index in place.
        let index = p.copy(run).join(".travsr");
        std::fs::create_dir_all(&index).unwrap();
        std::fs::write(index.join("graph.db"), "").unwrap();
        lock(&p.served).runs.insert(run);
        let err = p.request(run, "tools/list", json!({})).await.unwrap_err();
        assert_eq!(err, NOT_STARTED);
    }

    #[tokio::test]
    async fn a_server_that_cannot_start_tells_the_agent_no_host_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = missing(dir.path());
        let run = RunId::new();
        lock(&p.served).runs.insert(run);
        // No index: the reason names the copy's path, which stays on the host.
        let err = p.request(run, "tools/list", json!({})).await.unwrap_err();
        assert_eq!(err, NOT_STARTED);
        // An index, but no command to run: the same.
        let index = p.copy(run).join(".travsr");
        std::fs::create_dir_all(&index).unwrap();
        std::fs::write(index.join("graph.db"), "").unwrap();
        let err = p.request(run, "tools/list", json!({})).await.unwrap_err();
        assert_eq!(err, NOT_STARTED);
        let root = dir.path().display().to_string();
        assert!(!NOT_STARTED.contains(&root));
    }

    #[test]
    fn only_an_absolute_path_or_a_bare_name_is_a_command() {
        let rooted = std::env::temp_dir().join("travsr");
        for ok in [
            Path::new("travsr"),
            Path::new("travsr.exe"),
            rooted.as_path(),
        ] {
            assert!(acceptable_command(ok), "{}", ok.display());
        }
        for bad in ["", "./travsr", "bin/travsr", "../travsr", "a/../travsr"] {
            assert!(!acceptable_command(Path::new(bad)), "{bad}");
        }
        // On Windows `\travsr` has a root but no drive: it is relative to the current drive.
        #[cfg(windows)]
        {
            let no_drive = Path::new(r"\travsr");
            assert!(!acceptable_command(no_drive));
            assert!(resolve_command(no_drive, None).is_err());
            let p = TravsrProcesses::new(
                no_drive.to_path_buf(),
                std::env::temp_dir(),
                Arc::new(FakeGit),
                Duration::from_secs(1),
            );
            assert!(p.command(["init"]).is_err());
        }
    }

    /// What the real-Travsr test does: run the binary, or skip, saying why.
    #[derive(Debug)]
    enum RealTravsr {
        Run(PathBuf),
        Skip(String),
    }

    /// The travsr the real-Travsr test runs. `wanted` is `TRAVSR_BIN` (default `travsr`),
    /// resolved on `path` as `serve` resolves `travsr.command`; `version` runs `--version`.
    /// `pinned` is `KESTREL_TRAVSR_VERSION`: without it a travsr that is missing or does not
    /// run is a `Skip`; with it, as in CI, that is an `Err`, and so is any other version.
    fn real_travsr(
        wanted: Option<std::ffi::OsString>,
        path: Option<&OsStr>,
        pinned: Option<&str>,
        version: impl Fn(&Path) -> io::Result<String>,
    ) -> Result<RealTravsr, String> {
        let wanted = wanted.map_or_else(|| PathBuf::from("travsr"), PathBuf::from);
        if !acceptable_command(&wanted) {
            return Err(format!(
                "TRAVSR_BIN must be an absolute path or a bare command name, not {}",
                wanted.display()
            ));
        }
        let missing = |why: String| match pinned {
            Some(v) => Err(format!("{why}, but KESTREL_TRAVSR_VERSION is {v}")),
            None => Ok(RealTravsr::Skip(format!("{why} (set TRAVSR_BIN)"))),
        };
        let Ok(bin) = resolve_command(&wanted, path) else {
            return missing("no travsr binary in an absolute PATH entry".into());
        };
        let Ok(out) = version(&bin) else {
            return missing(format!("{} does not run", bin.display()));
        };
        if let Some(v) = pinned
            && !out
                .split_whitespace()
                .any(|w| w.trim_start_matches('v') == v)
        {
            return Err(format!(
                "{} reports {:?}, not the pinned version {v}",
                bin.display(),
                out.trim()
            ));
        }
        Ok(RealTravsr::Run(bin))
    }

    // Issue #96, scenario 2: the real-Travsr test picks its binary with `real_travsr`. With no
    // pinned version (a developer's machine) a missing or broken travsr skips it; with one (CI)
    // that is a failure, and so is any other version than the pinned one.

    /// A `--version` that reports `out`.
    fn reports(out: &'static str) -> impl Fn(&Path) -> io::Result<String> {
        move |_| Ok(out.to_string())
    }

    /// A `--version` whose binary cannot be started.
    fn does_not_run(_: &Path) -> io::Result<String> {
        Err(io::Error::from(io::ErrorKind::NotFound))
    }

    #[test]
    fn real_travsr_missing_from_path_skips_unpinned_and_fails_pinned() {
        let empty = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([empty.path()]).unwrap();
        let picked = real_travsr(None, Some(&path), None, reports("travsr 1.4.0"));
        assert!(
            matches!(picked, Ok(RealTravsr::Skip(ref why)) if why.contains("set TRAVSR_BIN")),
            "{picked:?}"
        );
        let err =
            real_travsr(None, Some(&path), Some("1.4.0"), reports("travsr 1.4.0")).unwrap_err();
        assert!(err.contains("KESTREL_TRAVSR_VERSION"), "{err}");
        // No PATH at all is the same as an empty one.
        let picked = real_travsr(None, None, None, reports("travsr 1.4.0"));
        assert!(matches!(picked, Ok(RealTravsr::Skip(_))), "{picked:?}");
    }

    #[test]
    fn real_travsr_that_does_not_run_skips_unpinned_and_fails_pinned() {
        let bin = std::env::temp_dir().join("kestrel-no-such-travsr");
        let picked = real_travsr(Some(bin.clone().into()), None, None, does_not_run);
        assert!(
            matches!(picked, Ok(RealTravsr::Skip(ref why)) if why.contains("does not run")),
            "{picked:?}"
        );
        let err = real_travsr(Some(bin.into()), None, Some("1.4.0"), does_not_run).unwrap_err();
        assert!(err.contains("does not run"), "{err}");
    }

    #[test]
    fn real_travsr_runs_only_the_pinned_version_when_one_is_pinned() {
        let bin = std::env::temp_dir().join("travsr");
        let run = |out| real_travsr(Some(bin.clone().into()), None, Some("1.4.0"), reports(out));
        for ok in ["travsr 1.4.0", "travsr v1.4.0\n", "1.4.0"] {
            assert!(
                matches!(run(ok), Ok(RealTravsr::Run(ref b)) if *b == bin),
                "{ok:?}"
            );
        }
        for other in ["travsr 1.3.0", "travsr 11.4.0", "travsr 1.4.0-rc1", ""] {
            let err = run(other).unwrap_err();
            assert!(err.contains("1.4.0"), "{other:?}: {err}");
        }
        // Unpinned, any version runs.
        let picked = real_travsr(
            Some(bin.clone().into()),
            None,
            None,
            reports("travsr 0.1.0"),
        );
        assert!(
            matches!(picked, Ok(RealTravsr::Run(ref b)) if *b == bin),
            "{picked:?}"
        );
    }

    #[test]
    fn real_travsr_refuses_a_relative_travsr_bin_pinned_or_not() {
        for pinned in [None, Some("1.4.0")] {
            let err = real_travsr(
                Some("./travsr".into()),
                None,
                pinned,
                reports("travsr 1.4.0"),
            )
            .unwrap_err();
            assert!(err.contains("TRAVSR_BIN"), "{err}");
        }
    }

    /// `target` as a path relative to the current directory (`..` up to the deepest common
    /// ancestor, then down), `None` when they share no root (another drive, on Windows).
    fn relative_to_current_dir(target: &Path) -> Option<PathBuf> {
        let here = std::fs::canonicalize(std::env::current_dir().ok()?).ok()?;
        let target = std::fs::canonicalize(target).ok()?;
        let common = here
            .components()
            .zip(target.components())
            .take_while(|(a, b)| a == b)
            .count();
        if common == 0 {
            return None;
        }
        let mut relative = PathBuf::new();
        for _ in common..here.components().count() {
            relative.push("..");
        }
        relative.extend(target.components().skip(common));
        Some(relative)
    }

    #[test]
    fn a_bare_command_is_found_in_an_absolute_path_entry_only() {
        let name = "kestrel-test-travsr";
        let found = tempfile::Builder::new()
            .prefix("resolve")
            .tempdir()
            .unwrap();
        let absolute = std::fs::canonicalize(found.path()).unwrap();
        std::fs::write(absolute.join(name), "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(absolute.join(name), mode).unwrap();
        }
        let empty = tempfile::tempdir().unwrap();
        // The same directory by a relative entry, which does reach the file from here, is
        // passed over.
        if let Some(relative) = relative_to_current_dir(&absolute) {
            assert!(relative.is_relative() && runnable(&relative.join(name)));
            let only_relative = std::env::join_paths([&relative]).unwrap();
            assert!(resolve_command(Path::new(name), Some(&only_relative)).is_err());
            let path =
                std::env::join_paths([relative, empty.path().into(), absolute.clone()]).unwrap();
            assert_eq!(
                resolve_command(Path::new(name), Some(&path)).unwrap(),
                absolute.join(name)
            );
        } else {
            eprintln!("SKIPPED the relative entry: the temp dir is on another drive");
        }
        let path = std::env::join_paths([empty.path(), absolute.as_path()]).unwrap();
        assert_eq!(
            resolve_command(Path::new(name), Some(&path)).unwrap(),
            absolute.join(name)
        );
        // Not found anywhere, or no PATH: an error naming neither.
        for path in [Some(path.as_os_str()), None] {
            let err = resolve_command(Path::new("kestrel-no-such-travsr"), path).unwrap_err();
            assert!(err.contains("travsr.command"), "{err}");
            assert!(
                !err.contains("kestrel-no-such") && !err.contains("resolve"),
                "{err}"
            );
        }
        // A relative path is never looked up.
        let err = resolve_command(Path::new("./kestrel-test-travsr"), Some(&path)).unwrap_err();
        assert!(err.contains("bare command name"), "{err}");
        // An absolute path is used as it is.
        let rooted = empty.path().join("anything");
        assert_eq!(resolve_command(&rooted, None).unwrap(), rooted);
        // On Unix a file that cannot be run is passed over.
        #[cfg(unix)]
        {
            std::fs::write(empty.path().join("kestrel-plain-file"), "").unwrap();
            let path = std::env::join_paths([empty.path()]).unwrap();
            assert!(resolve_command(Path::new("kestrel-plain-file"), Some(&path)).is_err());
        }
    }

    /// Gives `data`, then fails.
    struct Failing(&'static [u8]);

    impl tokio::io::AsyncRead for Failing {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            if self.0.is_empty() {
                return std::task::Poll::Ready(Err(io::Error::other("broken pipe")));
            }
            buf.put_slice(self.0);
            self.0 = &[];
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn only_the_latest_output_is_kept() {
        let long: Vec<u8> = (0..20_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let (kept, ended) = tail_of(Some(&long[..])).await;
        assert!(ended);
        assert_eq!(kept.len(), KEPT_OUTPUT);
        assert_eq!(kept, &long[long.len() - KEPT_OUTPUT..]);
        assert_eq!(tail_of::<&[u8]>(None).await, (Vec::new(), true));
        assert_eq!(
            tail_of(Some(&b"short"[..])).await,
            (b"short".to_vec(), true)
        );
        // A read error ends it early, not at the end of the output.
        assert_eq!(
            tail_of(Some(Failing(b"partial"))).await,
            (b"partial".to_vec(), false)
        );
        // The reason keeps the last words, whole characters only.
        let said = format!("{}\n", "é".repeat(400));
        let words = last_words(said.as_bytes());
        assert!(
            words.len() <= TAIL && words.chars().all(|c| c == 'é'),
            "{words}"
        );
        assert_eq!(last_words(b"  boom \n"), "boom");
    }

    #[test]
    fn a_committed_index_is_deleted_before_init() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path();
        // None: nothing to do.
        clear_committed_index(copy).unwrap();
        // A directory, whatever it holds.
        std::fs::create_dir_all(copy.join(".travsr/sub")).unwrap();
        std::fs::write(copy.join(".travsr/graph.db"), "forged").unwrap();
        clear_committed_index(copy).unwrap();
        assert!(!copy.join(".travsr").exists());
        // A file.
        std::fs::write(copy.join(".travsr"), "x").unwrap();
        clear_committed_index(copy).unwrap();
        assert!(!copy.join(".travsr").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_committed_index_that_is_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (copy, elsewhere) = (dir.path().join("copy"), dir.path().join("elsewhere"));
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep"), "").unwrap();
        std::os::unix::fs::symlink(&elsewhere, copy.join(".travsr")).unwrap();
        let err = clear_committed_index(&copy).unwrap_err();
        assert!(err.contains("is a link"), "{err}");
        // Nothing behind it was touched.
        assert!(elsewhere.join("keep").exists());
        // A link to nothing is refused too.
        std::fs::remove_file(copy.join(".travsr")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("none"), copy.join(".travsr")).unwrap();
        assert!(clear_committed_index(&copy).is_err());
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
        ///   line in `<dir>/registry`). It writes its index to `.travsr/graph.db` as it finds it,
        ///   so through a `.travsr` link too, and keeps what a `.travsr` directory holds. It adds
        ///   a line to `.gitignore` and writes `.travsrignore` when none exists, both through a
        ///   link too (a link to nothing counts as none).
        /// * `mcp` starts a daemon without `CI`, then answers `tools/list`, echoes every
        ///   `tools/call` as its text, and for tools named `die`, `quit`, `hang` and `bad` dies,
        ///   answers then quits, never answers, or answers with an error.
        ///
        /// Flag files in `<dir>`: `init_fails`; `init_sleeps`, with which `init` first starts a
        /// helper in the background that writes `helper_done` after 2 s, then writes
        /// `init_started`, sleeps 2 s and writes `init_done` (both only if not killed first);
        /// `init_detaches`, with which `init` starts a helper that lets go of its output and
        /// writes `detached_done` after 1 s, then ends at once.
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
    if [ -f "$d/init_sleeps" ]; then
      (sleep 2; : > "$d/helper_done") &
      echo $$ > "$d/init_started"
      sleep 2
      : > "$d/init_done"
    fi
    if [ -f "$d/init_detaches" ]; then
      (sleep 1; : > "$d/detached_done") < /dev/null > /dev/null 2>&1 &
    fi
    [ -n "$TRAVSR_SKIP_DOWNLOAD" ] || echo "download in $(pwd -P)" >> "$d/downloads"
    [ "$TRAVSR_DISABLE_REGISTRY" = 1 ] || echo "$(pwd -P)" >> "$d/registry"
    mkdir -p .travsr .git/hooks && : > .travsr/graph.db
    echo 'travsr hook-run' > .git/hooks/post-commit
    echo '.travsr/' >> .gitignore
    [ -e .travsrignore ] || echo '# travsr' > .travsrignore
    case " $* " in
      *" --no-connect "*) ;;
      *) echo '{}' > .mcp.json; echo travsr > CLAUDE.md; echo travsr > AGENTS.md ;;
    esac
    daemon
    echo "indexed"
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
        use kestrel_sandbox::{KillOnDropRunner, ProcessRunner};
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

        /// Commits everything in `repo` as it is, and returns the commit.
        fn commit_all(repo: &Path) -> String {
            git(repo, &["add", "-A"]);
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
                    "all",
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
                Arc::new(KillOnDropRunner),
                Duration::from_secs(30),
            );
            p.stop_timeout = Duration::from_secs(10);
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
            assert_eq!(lines(s.fake(), "registry"), [at.as_str()]);
            // Outside a git repo, init fails.
            let out = Std::new(&s.p.command)
                .arg("init")
                .current_dir(s.fake())
                .output()
                .unwrap();
            assert!(!out.status.success());
            // A `.travsr` link in the repo is written through, and so are a `.gitignore` link
            // and a `.travsrignore` link to nothing.
            let elsewhere = s.fake().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::fs::remove_dir_all(s.workspace.join(".travsr")).unwrap();
            std::os::unix::fs::symlink(&elsewhere, s.workspace.join(".travsr")).unwrap();
            let host_ignore = s.fake().join("host-gitignore");
            std::fs::write(&host_ignore, "host\n").unwrap();
            let dangling = s.fake().join("host-travsrignore");
            for name in [".gitignore", ".travsrignore"] {
                std::fs::remove_file(s.workspace.join(name)).unwrap();
            }
            std::os::unix::fs::symlink(&host_ignore, s.workspace.join(".gitignore")).unwrap();
            std::os::unix::fs::symlink(&dangling, s.workspace.join(".travsrignore")).unwrap();
            let out = Std::new(&s.p.command)
                .arg("init")
                .current_dir(&s.workspace)
                .output()
                .unwrap();
            assert!(out.status.success());
            assert!(elsewhere.join("graph.db").exists());
            assert_ne!(std::fs::read_to_string(&host_ignore).unwrap(), "host\n");
            assert!(dangling.exists());
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
        async fn s2_a_dropped_index_kills_init_and_the_helpers_it_started() {
            let s = setup();
            std::fs::write(s.fake().join("init_sleeps"), "").unwrap();
            let run = RunId::new();
            let source = at(&s.repo, Some(&s.first));
            let started = s.fake().join("init_started");
            // Driven until `init` has started its helper and said so (however long the clone
            // before it takes), then dropped, as the engine's timeout does.
            let mut index = Box::pin(s.p.index(run, &source));
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !started.exists() {
                assert!(std::time::Instant::now() < deadline, "init never started");
                tokio::select! {
                    done = &mut index => panic!("index finished before it was dropped: {done:?}"),
                    () = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
            drop(index);
            // Both would have written by now (2 s after they started) had they not been killed.
            tokio::time::sleep(Duration::from_secs(4)).await;
            assert!(!s.fake().join("init_done").exists(), "init was not killed");
            assert!(!s.fake().join("helper_done").exists(), "its helper was not");
            assert!(!s.p.copy(run).join(".travsr").exists());
            assert!(!s.p.dir(run).join("indexed").exists());
        }

        #[tokio::test]
        async fn a_committed_travsr_link_is_a_plain_file_in_the_copy_and_nothing_goes_through_it() {
            let s = setup();
            let elsewhere = s.fake().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, s.repo.join(".travsr")).unwrap();
            let linked = commit_all(&s.repo);
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&linked))).await.unwrap();
            // The copy held a plain file, deleted before `init` made its own index.
            let index = s.p.copy(run).join(".travsr");
            assert!(std::fs::symlink_metadata(&index).unwrap().is_dir());
            assert!(index.join("graph.db").is_file());
            assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
        }

        /// `travsr init` rewrites `.gitignore` and writes `.travsrignore` when there is none,
        /// through a link too. Committed links are plain files in the copy, so the host files
        /// they point at are never written.
        #[tokio::test]
        async fn committed_ignore_file_links_are_plain_files_so_init_writes_nothing_through_them() {
            let s = setup();
            let host = s.fake().join("host");
            std::fs::create_dir_all(&host).unwrap();
            let gitignore = host.join("gitignore");
            std::fs::write(&gitignore, "host\n").unwrap();
            let dangling = host.join("travsrignore");
            std::os::unix::fs::symlink(&gitignore, s.repo.join(".gitignore")).unwrap();
            std::os::unix::fs::symlink(&dangling, s.repo.join(".travsrignore")).unwrap();
            let linked = commit_all(&s.repo);
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&linked))).await.unwrap();
            assert_eq!(calls_of(s.fake(), "init").len(), 1);
            assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "host\n");
            assert!(!dangling.exists());
            let copy = s.p.copy(run);
            for name in [".gitignore", ".travsrignore"] {
                let meta = std::fs::symlink_metadata(copy.join(name)).unwrap();
                assert!(meta.is_file(), "{name}");
            }
            // `init` wrote the copy's own `.gitignore`.
            let written = std::fs::read_to_string(copy.join(".gitignore")).unwrap();
            assert!(written.ends_with(".travsr/\n"), "{written}");
        }

        /// A command that ends by itself, its output read to the end, has its group left
        /// alone: the group's id may belong to someone else by then. A helper that let go of
        /// the output keeps running.
        #[tokio::test]
        async fn a_clean_init_exit_does_not_kill_its_group() {
            let s = setup();
            std::fs::write(s.fake().join("init_detaches"), "").unwrap();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            let done = s.fake().join("detached_done");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !done.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the helper was killed"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        #[tokio::test]
        async fn a_committed_travsr_directory_is_deleted_before_init() {
            let s = setup();
            std::fs::create_dir_all(s.repo.join(".travsr")).unwrap();
            std::fs::write(s.repo.join(".travsr/graph.db"), "forged").unwrap();
            std::fs::write(s.repo.join(".travsr/extra"), "x").unwrap();
            let forged = commit_all(&s.repo);
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&forged))).await.unwrap();
            let index = s.p.copy(run).join(".travsr");
            assert!(!index.join("extra").exists());
            // The index is the one `init` wrote, not the commit's.
            assert_eq!(std::fs::read(index.join("graph.db")).unwrap(), b"");
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
            // The agent is told no host path; Kestrel's log has the reason.
            let err = s.p.request(run, "tools/list", json!({})).await.unwrap_err();
            assert_eq!(err, NOT_STARTED);
            assert!(calls_of(s.fake(), "mcp").is_empty());
        }

        #[tokio::test]
        async fn s8_forget_stops_the_server_and_deletes_the_copy_running_no_travsr() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &at(&s.repo, Some(&s.first))).await.unwrap();
            s.p.request(run, "tools/list", json!({})).await.unwrap();
            assert!(lock(&s.p.served).servers.contains_key(&run));
            let before = snapshot(&s.workspace);
            let ran = calls(s.fake()).len();
            s.p.forget(run).await.unwrap();
            assert!(!lock(&s.p.served).servers.contains_key(&run));
            // Nothing was registered, so no Travsr command runs.
            assert_eq!(calls(s.fake()).len(), ran);
            assert!(lines(s.fake(), "registry").is_empty());
            assert!(!s.p.dir(run).exists());
            assert_eq!(snapshot(&s.workspace), before);
            // Not served any more, and forgetting again is fine.
            let err = s.p.request(run, "tools/list", json!({})).await.unwrap_err();
            assert!(err.contains("not available"), "{err}");
            s.p.forget(run).await.unwrap();
            assert_eq!(calls(s.fake()).len(), ran);
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
            assert_eq!(calls(s.fake()).len(), 2, "init and mcp only");
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
            s.p.stop_timeout = Duration::from_secs(2);
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
                Arc::new(KillOnDropRunner),
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
        /// agent makes in its workspace never reaches the host's index. Against a real Travsr,
        /// picked by `real_travsr` from `TRAVSR_BIN` and `KESTREL_TRAVSR_VERSION`: skipped,
        /// saying so, when none is found or it does not run, unless a version is pinned, as CI
        /// does. `cargo test -p kestrel-cli --features integration`.
        #[cfg(feature = "integration")]
        #[tokio::test]
        async fn s12_the_index_answers_from_the_private_copy_and_never_sees_the_workspace() {
            let pinned = std::env::var("KESTREL_TRAVSR_VERSION").ok();
            let version = |bin: &Path| {
                let out = Std::new(bin).arg("--version").output()?;
                if !out.status.success() {
                    return Err(io::Error::other(format!("exit {}", out.status)));
                }
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            };
            let bin = match real_travsr(
                std::env::var_os("TRAVSR_BIN"),
                std::env::var_os("PATH").as_deref(),
                pinned.as_deref(),
                version,
            ) {
                Ok(RealTravsr::Run(bin)) => bin,
                Ok(RealTravsr::Skip(why)) => {
                    eprintln!("SKIPPED: {why}");
                    return;
                }
                Err(e) => panic!("{e}"),
            };
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
                Arc::new(KillOnDropRunner),
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
