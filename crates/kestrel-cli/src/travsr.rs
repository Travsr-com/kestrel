//! Travsr for `serve` (#44), when `kestrel.toml` has a `[travsr]` section.
//!
//! * [`TravsrProcesses`] runs the `travsr` command on the host. It indexes a run's clone with
//!   `travsr init` in the clone, keeps `.travsr/` out of the clone's git, and forgets the clone with
//!   `travsr repos --remove <path>`. Each run that calls a tool gets its own
//!   `travsr mcp --stdio --db <clone>/.travsr/graph.db`, started on its first call and reused.
//! * [`EngineTravsr`] is what the gateway asks: which tools a run may call (from the engine) and
//!   the calls themselves, always to the run's own clone.
//!
//! Travsr gets a short list of environment variables (see [`PASSED_ENV`]), never the run token or
//! a provider key.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use kestrel_core::{Engine, RunIndexer};
use kestrel_gateway::{ToolsError, TravsrHost};
use kestrel_types::RunId;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// The `[travsr]` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TravsrSettings {
    /// The travsr command: a path, or a name looked up on `PATH`.
    pub(crate) command: PathBuf,
    /// The longest indexing of one clone may take.
    pub(crate) index_timeout: Duration,
    /// The longest one tool call may take.
    pub(crate) call_timeout: Duration,
    /// Most clones indexed at once.
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

/// Environment variables Travsr gets from Kestrel's, besides those starting with `TRAVSR_`.
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
];

/// Most bytes of a failed command's output kept for its error.
const TAIL: usize = 500;
/// Longest line read from a Travsr server; a longer answer is an error.
const MAX_LINE: u64 = 16 * 1024 * 1024;

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

/// One run's server, behind a lock: a run's calls go one at a time.
type Slot = Arc<tokio::sync::Mutex<Option<Server>>>;

/// Runs the travsr command for the engine and the gateway.
pub(crate) struct TravsrProcesses {
    command: PathBuf,
    servers: Mutex<HashMap<RunId, Slot>>,
}

impl TravsrProcesses {
    pub(crate) fn new(command: PathBuf) -> TravsrProcesses {
        TravsrProcesses {
            command,
            servers: Mutex::default(),
        }
    }

    /// The travsr command with `args`, a clean environment and no stdin. Killed when dropped.
    fn command<S: AsRef<OsStr>>(&self, args: impl IntoIterator<Item = S>) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.args(args)
            .env_clear()
            .envs(std::env::vars_os().filter(|(name, _)| {
                name.to_str()
                    .is_some_and(|n| PASSED_ENV.contains(&n) || n.starts_with("TRAVSR_"))
            }))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }

    /// Runs `cmd` to the end. `Err` names `what` and ends with the tail of its output.
    async fn run(&self, mut cmd: Command, what: &str) -> Result<(), String> {
        let out = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
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

    /// `run`'s slot, made if it has none.
    fn slot(&self, run: RunId) -> Slot {
        lock(&self.servers).entry(run).or_default().clone()
    }

    /// Stops `run`'s server, if it has one.
    async fn stop(&self, run: RunId) {
        let slot = lock(&self.servers).remove(&run);
        if let Some(slot) = slot {
            slot.lock().await.take();
        }
    }

    /// Asks `run`'s server, for its `clone`, starting it if needed. A server that died, or that
    /// a dropped call left halfway, is started again.
    pub(crate) async fn request(
        &self,
        run: RunId,
        clone: &Path,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let slot = self.slot(run);
        let mut held = slot.lock().await;
        if held.as_mut().is_some_and(|s| s.busy || s.dead()) {
            held.take();
        }
        let server = match held.as_mut() {
            Some(server) => server,
            None => held.insert(self.start(clone)?),
        };
        match server.ask(method, params).await {
            Ok(answer) => answer,
            Err(Broken(why)) => {
                held.take();
                Err(why)
            }
        }
    }

    /// Starts a server for `clone`'s index.
    fn start(&self, clone: &Path) -> Result<Server, String> {
        let db = clone.join(".travsr").join("graph.db");
        let mut child = self
            .command([OsStr::new("mcp"), OsStr::new("--stdio"), OsStr::new("--db")])
            .arg(&db)
            .current_dir(clone)
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
}

#[async_trait]
impl RunIndexer for TravsrProcesses {
    async fn index(&self, run: RunId, clone: &Path) -> Result<(), String> {
        // A server of an earlier index would not see the new one.
        self.stop(run).await;
        let mut init = self.command(["init"]);
        init.current_dir(clone);
        self.run(init, "init").await?;
        exclude_index(clone).map_err(|e| {
            format!("could not keep .travsr/ out of the clone's git (.git/info/exclude): {e}")
        })
    }

    async fn forget(&self, run: RunId, clone: &Path) -> Result<(), String> {
        self.stop(run).await;
        // Travsr registers a repo under its canonical path.
        let path = std::fs::canonicalize(clone).unwrap_or_else(|_| clone.to_path_buf());
        let remove = self.command([
            OsStr::new("repos"),
            OsStr::new("--remove"),
            path.as_os_str(),
        ]);
        self.run(remove, "repos --remove").await
    }
}

/// Adds `.travsr/` to `<clone>/.git/info/exclude`, so the agent never commits the index. Nothing
/// is written through a link: a `.git`, `info` or `exclude` that is not a plain directory or
/// file is left alone.
fn exclude_index(clone: &Path) -> io::Result<()> {
    let git = clone.join(".git");
    if !std::fs::symlink_metadata(&git)?.is_dir() {
        return Ok(());
    }
    let info = git.join("info");
    match std::fs::symlink_metadata(&info) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir(&info)?,
        Err(e) => return Err(e),
    }
    let file = info.join("exclude");
    let existing = match std::fs::symlink_metadata(&file) {
        Ok(meta) if meta.is_file() => std::fs::read_to_string(&file)?,
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if existing
        .lines()
        .any(|l| matches!(l.trim(), ".travsr/" | ".travsr" | "/.travsr/" | "/.travsr"))
    {
        return Ok(());
    }
    let gap = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut out = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&file)?;
    write!(out, "{gap}# Travsr's index, made by Kestrel\n.travsr/\n")
}

/// The gateway's Travsr host: the engine says what a run may call and where its clone is.
pub(crate) struct EngineTravsr {
    pub(crate) engine: Arc<Engine>,
    pub(crate) processes: Arc<TravsrProcesses>,
    pub(crate) call_timeout: Duration,
}

impl EngineTravsr {
    fn clone_of(&self, run: RunId) -> Result<PathBuf, String> {
        self.engine
            .travsr_clone(run)
            .ok_or_else(|| "Travsr is not available for this run".to_string())
    }
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
        let clone = self.clone_of(run)?;
        let listed = self
            .processes
            .request(run, &clone, "tools/list", json!({}))
            .await?;
        listed
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| "Travsr listed no tools".to_string())
    }

    async fn call(&self, run: RunId, tool: &str, args: Value) -> Result<Value, String> {
        let clone = self.clone_of(run)?;
        let params = json!({ "name": tool, "arguments": args });
        self.processes
            .request(run, &clone, "tools/call", params)
            .await
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

    #[test]
    fn the_defaults_are_travsr_on_path_5_minutes_30_seconds_and_2_jobs() {
        let d = TravsrSettings::default();
        assert_eq!(d.command, PathBuf::from("travsr"));
        assert_eq!(d.index_timeout, Duration::from_secs(300));
        assert_eq!(d.call_timeout, Duration::from_secs(30));
        assert_eq!(d.max_indexing, 2);
    }

    fn clone_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("clone");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        (dir, clone)
    }

    fn exclude_of(clone: &Path) -> String {
        std::fs::read_to_string(clone.join(".git/info/exclude")).unwrap()
    }

    #[test]
    fn the_index_is_kept_out_of_git_once() {
        let (_dir, clone) = clone_dir();
        exclude_index(&clone).unwrap();
        exclude_index(&clone).unwrap();
        let text = exclude_of(&clone);
        assert_eq!(text.matches(".travsr/").count(), 1, "{text}");
        // An existing file keeps what it has, and gets the line on a line of its own.
        let (_dir, clone) = clone_dir();
        std::fs::create_dir_all(clone.join(".git/info")).unwrap();
        std::fs::write(clone.join(".git/info/exclude"), "*.log").unwrap();
        exclude_index(&clone).unwrap();
        assert_eq!(
            exclude_of(&clone),
            "*.log\n# Travsr's index, made by Kestrel\n.travsr/\n"
        );
        // Already excluded another way: left as it is.
        std::fs::write(clone.join(".git/info/exclude"), "/.travsr\n").unwrap();
        exclude_index(&clone).unwrap();
        assert_eq!(exclude_of(&clone), "/.travsr\n");
    }

    #[test]
    fn a_git_entry_that_is_not_a_plain_directory_or_file_is_left_alone() {
        // A gitfile (`.git` is a file): nothing is written.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        exclude_index(dir.path()).unwrap();
        assert!(!dir.path().join("info").exists());
        // `info` is a file, or `exclude` a directory: left alone.
        let (_dir, clone) = clone_dir();
        std::fs::write(clone.join(".git/info"), "x").unwrap();
        exclude_index(&clone).unwrap();
        assert_eq!(
            std::fs::read_to_string(clone.join(".git/info")).unwrap(),
            "x"
        );
        let (_dir, clone) = clone_dir();
        std::fs::create_dir_all(clone.join(".git/info/exclude")).unwrap();
        exclude_index(&clone).unwrap();
        assert!(clone.join(".git/info/exclude").is_dir());
        // No `.git` at all is an error.
        let dir = tempfile::tempdir().unwrap();
        assert!(exclude_index(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_exclude_file_that_is_a_link_is_never_written_through() {
        let (dir, clone) = clone_dir();
        let target = dir.path().join("target");
        std::fs::write(&target, "mine\n").unwrap();
        std::fs::create_dir_all(clone.join(".git/info")).unwrap();
        std::os::unix::fs::symlink(&target, clone.join(".git/info/exclude")).unwrap();
        exclude_index(&clone).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine\n");
        // Nor an `info` that is a link to a directory.
        let (dir, clone) = clone_dir();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, clone.join(".git/info")).unwrap();
        exclude_index(&clone).unwrap();
        assert!(!elsewhere.join("exclude").exists());
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_is_an_error_naming_it() {
        let (dir, clone) = clone_dir();
        let missing = dir.path().join("no-such-travsr");
        let p = TravsrProcesses::new(missing.clone());
        let err = p.index(RunId::new(), &clone).await.unwrap_err();
        assert!(
            err.contains("could not start") && err.contains("no-such-travsr"),
            "{err}"
        );
        let err = p
            .request(RunId::new(), &clone, "tools/list", json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("could not start"), "{err}");
    }

    /// A fake `travsr` (a shell script) and what it wrote down.
    #[cfg(unix)]
    pub(crate) mod fake {
        use std::path::{Path, PathBuf};

        /// The script. It writes to `<dir>/log`, and reads flag files in `<dir>`: `init_fails`,
        /// `init_sleeps`, `remove_fails`. Its server answers `tools/list`, echoes every
        /// `tools/call` as its text, and for tools named `die`, `quit`, `hang` and `bad` dies,
        /// answers then quits, never answers, or answers with an error.
        const SCRIPT: &str = r#"#!/bin/sh
d='@DIR@'
case "$1" in
  init)
    echo "init $(pwd -P)" >> "$d/log"
    if [ -f "$d/init_fails" ]; then echo "indexing 3 files"; echo "boom: no space left" >&2; exit 3; fi
    if [ -f "$d/init_sleeps" ]; then sleep 30; fi
    mkdir -p .travsr && : > .travsr/graph.db
    echo "indexed"
    ;;
  repos)
    echo "repos $2 $3" >> "$d/log"
    if [ -f "$d/remove_fails" ]; then echo "not registered" >&2; exit 1; fi
    ;;
  mcp)
    echo "mcp $2 $3 $4 $(pwd -P)" >> "$d/log"
    env > "$d/env"
    while IFS= read -r line; do
      id=$(printf '%s\n' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
      echo "req $line" >> "$d/log"
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
                match std::process::Command::new(&path).arg("--version").status() {
                    Err(e) if e.raw_os_error() == Some(26) => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    _ => break,
                }
            }
            path
        }

        pub(crate) fn log(dir: &Path) -> Vec<String> {
            std::fs::read_to_string(dir.join("log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        pub(crate) fn count(dir: &Path, prefix: &str) -> usize {
            log(dir).iter().filter(|l| l.starts_with(prefix)).count()
        }
    }

    #[cfg(unix)]
    mod with_fake {
        use super::fake::{count, log, travsr};
        use super::*;

        struct Setup {
            dir: tempfile::TempDir,
            clone: PathBuf,
            p: TravsrProcesses,
        }

        fn setup() -> Setup {
            let dir = tempfile::tempdir().unwrap();
            let clone = dir.path().join("clone");
            std::fs::create_dir_all(clone.join(".git")).unwrap();
            let p = TravsrProcesses::new(travsr(dir.path()));
            Setup { dir, clone, p }
        }

        fn real(path: &Path) -> String {
            std::fs::canonicalize(path).unwrap().display().to_string()
        }

        #[tokio::test]
        async fn s1_index_runs_init_in_the_clone_and_keeps_the_index_out_of_git() {
            let s = setup();
            s.p.index(RunId::new(), &s.clone).await.unwrap();
            assert_eq!(log(s.dir.path()), [format!("init {}", real(&s.clone))]);
            assert!(s.clone.join(".travsr/graph.db").exists());
            assert!(exclude_of(&s.clone).contains(".travsr/"));
        }

        #[tokio::test]
        async fn s2_a_failed_init_says_why_with_the_tail_of_its_output() {
            let s = setup();
            std::fs::write(s.dir.path().join("init_fails"), "").unwrap();
            let err = s.p.index(RunId::new(), &s.clone).await.unwrap_err();
            assert!(err.contains("`travsr init` exited with code 3"), "{err}");
            assert!(err.contains("boom: no space left"), "{err}");
            assert!(!err.contains("indexing 3 files"), "{err}");
        }

        #[tokio::test]
        async fn s2_a_dropped_index_kills_init() {
            let s = setup();
            std::fs::write(s.dir.path().join("init_sleeps"), "").unwrap();
            let gave_up = tokio::time::timeout(
                Duration::from_millis(300),
                s.p.index(RunId::new(), &s.clone),
            )
            .await;
            assert!(gave_up.is_err());
            // Killed: it never gets to make the index.
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!s.clone.join(".travsr").exists());
        }

        #[tokio::test]
        async fn s8_forget_removes_the_canonical_path_and_stops_the_server() {
            let s = setup();
            let run = RunId::new();
            s.p.index(run, &s.clone).await.unwrap();
            s.p.request(run, &s.clone, "tools/list", json!({}))
                .await
                .unwrap();
            assert!(lock(&s.p.servers).contains_key(&run));
            s.p.forget(run, &s.clone).await.unwrap();
            assert!(!lock(&s.p.servers).contains_key(&run));
            assert!(
                log(s.dir.path()).contains(&format!("repos --remove {}", real(&s.clone))),
                "{:?}",
                log(s.dir.path())
            );
            std::fs::write(s.dir.path().join("remove_fails"), "").unwrap();
            let err = s.p.forget(run, &s.clone).await.unwrap_err();
            assert!(
                err.contains("repos --remove") && err.contains("not registered"),
                "{err}"
            );
            // A clone that is gone is removed by the path it had.
            let gone = s.dir.path().join("gone");
            std::fs::remove_file(s.dir.path().join("remove_fails")).unwrap();
            s.p.forget(run, &gone).await.unwrap();
            assert!(log(s.dir.path()).contains(&format!("repos --remove {}", gone.display())));
        }

        #[tokio::test]
        async fn s3_calls_go_to_one_server_per_run_bound_to_its_clone() {
            let s = setup();
            let (a, b) = (RunId::new(), RunId::new());
            let other = s.dir.path().join("other");
            std::fs::create_dir_all(&other).unwrap();
            let listed =
                s.p.request(a, &s.clone, "tools/list", json!({}))
                    .await
                    .unwrap();
            assert_eq!(listed["tools"][0]["name"], "get_callers");
            let called =
                s.p.request(
                    a,
                    &s.clone,
                    "tools/call",
                    json!({ "name": "get_callers", "arguments": { "symbol": "x\"y\\z" } }),
                )
                .await
                .unwrap();
            let text = called["content"][0]["text"].as_str().unwrap();
            let sent: Value = serde_json::from_str(text).unwrap();
            assert_eq!(sent["params"]["arguments"]["symbol"], "x\"y\\z");
            s.p.request(b, &other, "tools/call", json!({ "name": "get_callers" }))
                .await
                .unwrap();
            // One server per run, each on its own clone's index and in its clone.
            let db = |c: &Path| format!("{}", c.join(".travsr").join("graph.db").display());
            let starts: Vec<String> = log(s.dir.path())
                .into_iter()
                .filter(|l| l.starts_with("mcp "))
                .collect();
            assert_eq!(
                starts,
                [
                    format!("mcp --stdio --db {} {}", db(&s.clone), real(&s.clone)),
                    format!("mcp --stdio --db {} {}", db(&other), real(&other)),
                ]
            );
        }

        #[tokio::test]
        async fn travsr_gets_no_secret_from_kestrel_s_environment() {
            let s = setup();
            s.p.request(RunId::new(), &s.clone, "tools/list", json!({}))
                .await
                .unwrap();
            let env = std::fs::read_to_string(s.dir.path().join("env")).unwrap();
            for line in env.lines() {
                let name = line.split('=').next().unwrap();
                let fine = PASSED_ENV.contains(&name)
                    || name.starts_with("TRAVSR_")
                    || ["PWD", "SHLVL", "_", "OLDPWD"].contains(&name);
                assert!(fine, "{name} reached travsr");
            }
        }

        #[tokio::test]
        async fn s7_a_server_that_dies_is_an_error_and_is_started_again() {
            let s = setup();
            let run = RunId::new();
            let err =
                s.p.request(run, &s.clone, "tools/call", json!({ "name": "die" }))
                    .await
                    .unwrap_err();
            assert!(err.contains("stopped"), "{err}");
            // Answers, then exits: the next call finds it dead and starts another.
            s.p.request(run, &s.clone, "tools/call", json!({ "name": "quit" }))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            s.p.request(
                run,
                &s.clone,
                "tools/call",
                json!({ "name": "get_callers" }),
            )
            .await
            .unwrap();
            assert_eq!(count(s.dir.path(), "mcp "), 3);
        }

        #[tokio::test]
        async fn s7_travsr_s_error_is_passed_on_and_the_server_kept() {
            let s = setup();
            let run = RunId::new();
            let err =
                s.p.request(run, &s.clone, "tools/call", json!({ "name": "bad" }))
                    .await
                    .unwrap_err();
            assert_eq!(err, "unknown tool bad");
            s.p.request(
                run,
                &s.clone,
                "tools/call",
                json!({ "name": "get_callers" }),
            )
            .await
            .unwrap();
            assert_eq!(count(s.dir.path(), "mcp "), 1);
        }

        #[tokio::test]
        async fn s7_a_call_given_up_on_leaves_no_stale_answer_behind() {
            let s = setup();
            let run = RunId::new();
            let gave_up = tokio::time::timeout(
                Duration::from_millis(300),
                s.p.request(run, &s.clone, "tools/call", json!({ "name": "hang" })),
            )
            .await;
            assert!(gave_up.is_err());
            // The server that may still answer the old call is replaced.
            let answer =
                s.p.request(
                    run,
                    &s.clone,
                    "tools/call",
                    json!({ "name": "get_callers" }),
                )
                .await
                .unwrap();
            assert!(
                answer["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("get_callers")
            );
            assert_eq!(count(s.dir.path(), "mcp "), 2);
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

        /// Git whose `clone` makes the target directory with a `.git` directory.
        struct FakeGit;

        #[async_trait]
        impl kestrel_sandbox::CommandRunner for FakeGit {
            async fn run(&self, argv: &[String]) -> io::Result<kestrel_sandbox::CommandOutput> {
                if argv.get(1).is_some_and(|a| a == "clone") {
                    std::fs::create_dir_all(Path::new(argv.last().unwrap()).join(".git"))?;
                }
                Ok(kestrel_sandbox::CommandOutput {
                    status: 0,
                    stdout: ".git\n".into(),
                    stderr: String::new(),
                })
            }
            async fn spawn(&self, _: &[String]) -> io::Result<kestrel_sandbox::SpawnedProcess> {
                Err(io::Error::other("never spawned"))
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

        /// Scenarios 1, 3, 5 and 9 end to end: a real engine indexes the run's clone with the
        /// fake travsr, and the gateway host reaches that clone's server only.
        #[tokio::test]
        async fn the_gateway_host_reaches_the_run_s_own_indexed_clone() {
            let s = setup();
            let repo = s.dir.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            let store = Arc::new(kestrel_store::Store::open_in_memory().unwrap());
            let processes = Arc::new(TravsrProcesses::new(travsr(s.dir.path())));
            let engine = Arc::new(
                Engine::new(
                    store.clone(),
                    vec![Box::new(Idle)],
                    Arc::new(kestrel_agent::adapter_for),
                    url::Url::parse("http://127.0.0.1:7070").unwrap(),
                    kestrel_core::Limits::default(),
                )
                .with_workspaces(kestrel_core::Workspaces::new(
                    s.dir.path().join("workspaces"),
                    Arc::new(FakeGit),
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
                .submit(spec(&repo, "[travsr.search_symbol]"), "read".into())
                .await
                .unwrap();
            let other = engine
                .submit(spec(&repo, "[kestrel.join]"), "other".into())
                .await
                .unwrap();
            for _ in 0..500 {
                if engine.travsr_clone(run).is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let clone = engine.travsr_clone(run).expect("the clone is indexed");
            assert_eq!(clone, s.dir.path().join("workspaces").join(run.to_string()));
            assert!(exclude_of(&clone).contains(".travsr/"));
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
            // Every server started is on this run's clone.
            for line in log(s.dir.path()).iter().filter(|l| l.starts_with("mcp ")) {
                assert!(line.contains(&run.to_string()), "{line}");
            }
            // The other run has no Travsr: no tools, and no clone to call.
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

        /// Scenario 12, against a real Travsr: `TRAVSR_BIN`, else `travsr` on `PATH`. Skipped,
        /// saying so, when neither runs. `cargo test -p kestrel-cli --features integration`.
        #[cfg(feature = "integration")]
        #[tokio::test]
        async fn s12_a_clone_edited_and_indexed_again_shows_its_own_change() {
            let bin = std::env::var_os("TRAVSR_BIN")
                .map_or_else(|| PathBuf::from("travsr"), PathBuf::from);
            if std::process::Command::new(&bin)
                .arg("--version")
                .output()
                .is_err()
            {
                eprintln!("SKIPPED: no travsr binary (set TRAVSR_BIN)");
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let clone = dir.path().join("clone");
            std::fs::create_dir_all(clone.join("src")).unwrap();
            let git = std::process::Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(&clone)
                .status()
                .unwrap();
            assert!(git.success());
            std::fs::write(
                clone.join("src/lib.rs"),
                "pub fn alpha_kestrel_marker() {}\n",
            )
            .unwrap();
            let p = TravsrProcesses::new(bin);
            let run = RunId::new();
            let find =
                |name: &str| json!({ "name": "search_symbol", "arguments": { "name": name } });
            p.index(run, &clone).await.unwrap();
            let found = p
                .request(run, &clone, "tools/call", find("alpha_kestrel_marker"))
                .await
                .unwrap();
            assert!(
                found.to_string().contains("alpha_kestrel_marker"),
                "{found}"
            );
            // The run edits its clone; once indexed again, Travsr sees the change.
            std::fs::write(
                clone.join("src/lib.rs"),
                "pub fn alpha_kestrel_marker() {}\npub fn beta_kestrel_marker() {}\n",
            )
            .unwrap();
            p.index(run, &clone).await.unwrap();
            let found = p
                .request(run, &clone, "tools/call", find("beta_kestrel_marker"))
                .await
                .unwrap();
            assert!(found.to_string().contains("beta_kestrel_marker"), "{found}");
            p.forget(run, &clone).await.unwrap();
        }

        #[tokio::test]
        async fn index_stops_the_run_s_old_server() {
            let s = setup();
            let run = RunId::new();
            s.p.request(run, &s.clone, "tools/list", json!({}))
                .await
                .unwrap();
            s.p.index(run, &s.clone).await.unwrap();
            assert!(!lock(&s.p.servers).contains_key(&run));
        }
    }
}
