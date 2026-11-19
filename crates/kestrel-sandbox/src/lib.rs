//! Sandbox drivers: gVisor on Linux, Apple container on macOS.
//!
//! Every process a driver starts goes through a [`CommandRunner`], so unit tests swap in a fake
//! and never spawn a real process.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use std::process::Stdio;

use async_trait::async_trait;
use futures::future::{BoxFuture, Shared};
use futures::{FutureExt, Stream};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use url::Url;

/// Errors from sandbox drivers and host detection.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// A sandbox CLI call exited non-zero. `stderr` is the CLI's own error text.
    #[error("`{program}` exited with code {code}: {stderr}")]
    CommandFailed {
        program: String,
        code: i32,
        stderr: String,
    },
    /// The runner could not start the process at all (binary missing, permissions, ...).
    #[error("could not run `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    /// No strong sandbox is available on this host and weak isolation was not allowed.
    #[error("no secure sandbox available on this host. {instructions}")]
    NoSecureSandbox { instructions: String },
    /// An env var that looks like a real secret was passed to the sandbox.
    #[error("refusing to launch: env var `{key}` looks like a real secret")]
    SecretInEnv { key: String },
    /// `wait` needs the exit status of a `start` this driver ran, and it never started `id`.
    /// Happens for example after an engine restart.
    #[error("sandbox `{id}` was not started by this driver, so its exit status is unknown")]
    NotAttached { id: String },
    /// The image is empty or starts with `-`, so the CLI would read it as a flag.
    #[error("invalid image `{image}`: must be non-empty and must not start with `-`")]
    InvalidImage { image: String },
    /// The workspace path is relative, not UTF-8, or has characters that would change the
    /// `--mount` options (`,`, `=`, `"` or a newline).
    #[error(
        "invalid workspace path `{path}`: must be absolute UTF-8 without `,` `=` `\"` or newlines"
    )]
    InvalidWorkspace { path: String },
    /// A container id that is empty, starts with `-`, or has characters outside
    /// `[A-Za-z0-9_.-]`.
    #[error("invalid container id `{id}`")]
    InvalidId { id: String },
}

pub type Result<T> = std::result::Result<T, SandboxError>;

/// Network access for a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No network at all.
    None,
    /// Only the Kestrel gateway is reachable.
    GatewayOnly { gateway: Url },
}

/// What to run and with which limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub image: String,
    /// Passed as separate arguments, never through a shell.
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Host directory mounted at `/workspace`.
    pub workspace: Option<PathBuf>,
    pub cpu_millis: u32,
    pub memory_mb: u64,
    pub network: NetworkPolicy,
}

/// A created sandbox. `id` is the container id printed by the CLI on create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxHandle {
    pub id: String,
}

/// Exit status of the sandboxed process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: i32,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// Which standard stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdStream {
    Stdout,
    Stderr,
}

/// One line of output, tagged by stream. `line` has no trailing newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    pub stream: StdStream,
    pub line: String,
}

/// Output lines in the order they were produced.
pub type OutputStream = Pin<Box<dyn Stream<Item = OutputLine> + Send>>;

/// Resolves to the exit code of a spawned process (-1 if it was killed by a signal).
pub type ExitFuture = Pin<Box<dyn Future<Output = std::io::Result<i32>> + Send>>;

/// A started process: its output lines, and its exit code once it ends.
/// Keep polling `output` while awaiting `exit`: a full pipe blocks the child, and dropping
/// `output` early can kill a child that still writes (SIGPIPE), so `exit` reports -1.
pub struct SpawnedProcess {
    pub output: OutputStream,
    pub exit: ExitFuture,
}

/// Result of a command run to completion.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// All process spawning goes through this. `argv[0]` is the program; nothing is run via a shell.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run to completion and capture output.
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput>;
    /// Start, stream stdout and stderr lines as they arrive, and report the exit code.
    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess>;
}

/// The real runner: spawns host processes. See [`SpawnedProcess`]: keep reading the output
/// stream while awaiting the exit, or the child can block on a full pipe or die with -1.
#[derive(Debug, Clone, Default)]
pub struct ProcessRunner;

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        let out = command(argv)?.output().await?;
        Ok(CommandOutput {
            // A process killed by a signal has no code.
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        let mut child = command(argv)?
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(std::io::Error::other("child has no stdout or stderr pipe"));
        };
        // Reap the child in the background so it never lingers as a zombie, even if nobody
        // awaits `exit`.
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let code = child.wait().await.map(|s| s.code().unwrap_or(-1));
            tx.send(code).ok();
        });
        Ok(SpawnedProcess {
            output: Box::pin(futures::stream::select(
                lines(out, StdStream::Stdout),
                lines(err, StdStream::Stderr),
            )),
            exit: Box::pin(async move { rx.await.map_err(std::io::Error::other)? }),
        })
    }
}

/// `argv[0]` with `argv[1..]` as separate arguments. No shell is involved.
fn command(argv: &[String]) -> std::io::Result<tokio::process::Command> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    Ok(cmd)
}

/// Lines read from `reader`, tagged with `stream`. Ends at EOF or the first read error.
fn lines<R>(reader: R, stream: StdStream) -> impl Stream<Item = OutputLine> + Send
where
    R: AsyncRead + Unpin + Send + 'static,
{
    futures::stream::unfold(BufReader::new(reader).lines(), move |mut it| async move {
        let line = it.next_line().await.ok().flatten()?;
        Some((OutputLine { stream, line }, it))
    })
}

#[async_trait]
pub trait SandboxDriver: Send + Sync {
    /// "gvisor", "apple-container" or "docker-runc".
    fn name(&self) -> &'static str;
    /// False only for docker-runc.
    fn is_strong(&self) -> bool;
    async fn create(&self, spec: &LaunchSpec) -> Result<SandboxHandle>;
    async fn start(&self, h: &SandboxHandle) -> Result<OutputStream>;
    async fn wait(&self, h: &SandboxHandle) -> Result<ExitStatus>;
    /// Idempotent.
    async fn stop(&self, h: &SandboxHandle) -> Result<()>;
    /// Idempotent.
    async fn destroy(&self, h: &SandboxHandle) -> Result<()>;
}

/// Name of the internal network used for `NetworkPolicy::GatewayOnly`.
const GATEWAY_NETWORK: &str = "kestrel-gw";

/// How one container CLI spells things. The three drivers differ only here.
struct Cli {
    name: &'static str,
    program: &'static str,
    strong: bool,
    /// `--runtime=...` for docker, nothing for Apple `container`.
    runtime: Option<&'static str>,
    /// Apple takes whole CPUs and an upper-case memory suffix.
    apple: bool,
    /// Text in stderr meaning the container is already gone.
    not_found: &'static str,
}

const GVISOR: Cli = Cli {
    name: "gvisor",
    program: "docker",
    strong: true,
    runtime: Some("--runtime=runsc"),
    apple: false,
    not_found: "No such container",
};

const APPLE: Cli = Cli {
    name: "apple-container",
    program: "container",
    strong: true,
    runtime: None,
    apple: true,
    not_found: "not found",
};

const DOCKER_RUNC: Cli = Cli {
    name: "docker-runc",
    program: "docker",
    strong: false,
    runtime: Some("--runtime=runc"),
    apple: false,
    not_found: "No such container",
};

impl Cli {
    fn argv(&self, args: &[&str]) -> Vec<String> {
        std::iter::once(self.program)
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect()
    }

    /// Runs `argv`. A non-zero exit whose stderr contains `tolerated` counts as success.
    async fn run(
        &self,
        runner: &dyn CommandRunner,
        argv: &[String],
        tolerated: Option<&str>,
    ) -> Result<String> {
        let out = runner
            .run(argv)
            .await
            .map_err(|source| SandboxError::Spawn {
                program: self.program.to_string(),
                source,
            })?;
        if out.status == 0 || tolerated.is_some_and(|t| out.stderr.contains(t)) {
            return Ok(out.stdout);
        }
        Err(SandboxError::CommandFailed {
            program: self.program.to_string(),
            code: out.status,
            stderr: out.stderr.trim_end().to_string(),
        })
    }

    fn cpus(&self, millis: u32) -> String {
        if self.apple {
            return millis.div_ceil(1000).max(1).to_string();
        }
        let frac = format!("{:03}", millis % 1000);
        match frac.trim_end_matches('0') {
            "" => (millis / 1000).to_string(),
            frac => format!("{}.{frac}", millis / 1000),
        }
    }

    fn create_argv(&self, spec: &LaunchSpec) -> Vec<String> {
        let suffix = if self.apple { "M" } else { "m" };
        let network = match spec.network {
            NetworkPolicy::None => "none",
            NetworkPolicy::GatewayOnly { .. } => GATEWAY_NETWORK,
        };
        let mut argv = self.argv(&["create"]);
        argv.extend(self.runtime.map(str::to_string));
        argv.extend([
            "--cpus".to_string(),
            self.cpus(spec.cpu_millis),
            "--memory".to_string(),
            format!("{}{suffix}", spec.memory_mb),
            "--network".to_string(),
            network.to_string(),
        ]);
        if let Some(ws) = &spec.workspace {
            argv.extend([
                "--mount".to_string(),
                format!("type=bind,source={},target=/workspace", ws.display()),
                "--workdir".to_string(),
                "/workspace".to_string(),
            ]);
        }
        let gateway = match &spec.network {
            NetworkPolicy::None => None,
            NetworkPolicy::GatewayOnly { gateway } => {
                Some(("KESTREL_GATEWAY_URL", gateway.as_str()))
            }
        };
        for (k, v) in spec
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain(gateway)
        {
            argv.extend(["--env".to_string(), format!("{k}={v}")]);
        }
        argv.push(spec.image.clone());
        argv.extend(spec.argv.iter().cloned());
        argv
    }

    async fn create(&self, runner: &dyn CommandRunner, spec: &LaunchSpec) -> Result<SandboxHandle> {
        check_secrets(&spec.env)?;
        check_image(&spec.image)?;
        if let Some(ws) = &spec.workspace {
            validate_workspace_path(ws)?;
        }
        if let NetworkPolicy::GatewayOnly { .. } = spec.network {
            let argv = self.argv(&["network", "create", "--internal", GATEWAY_NETWORK]);
            self.run(runner, &argv, Some("already exists")).await?;
        }
        let stdout = self.run(runner, &self.create_argv(spec), None).await?;
        Ok(SandboxHandle {
            id: check_id(stdout.trim())?.to_string(),
        })
    }

    async fn start(
        &self,
        runner: &dyn CommandRunner,
        exits: &Exits,
        h: &SandboxHandle,
    ) -> Result<OutputStream> {
        let argv = self.argv(&["start", "--attach", check_id(&h.id)?]);
        let process = runner
            .spawn(&argv)
            .await
            .map_err(|source| SandboxError::Spawn {
                program: self.program.to_string(),
                source,
            })?;
        if self.apple {
            exits.insert(&h.id, process.exit);
        }
        Ok(process.output)
    }

    async fn wait(
        &self,
        runner: &dyn CommandRunner,
        exits: &Exits,
        h: &SandboxHandle,
    ) -> Result<ExitStatus> {
        if self.apple {
            // The Apple CLI has no `wait` command and does not report exit codes after the fact,
            // so the exit code is the one of the `start --attach` process.
            let exit = exits
                .get(&h.id)
                .ok_or_else(|| SandboxError::NotAttached { id: h.id.clone() })?;
            let code = exit.await.map_err(|e| SandboxError::Spawn {
                program: self.program.to_string(),
                source: std::io::Error::new(e.kind(), e.to_string()),
            })?;
            return Ok(ExitStatus { code });
        }
        let argv = self.argv(&["wait", check_id(&h.id)?]);
        let stdout = self.run(runner, &argv, None).await?;
        let code = stdout
            .trim()
            .parse()
            .map_err(|_| SandboxError::CommandFailed {
                program: self.program.to_string(),
                code: 0,
                stderr: format!("unexpected `wait` output: {stdout:?}"),
            })?;
        Ok(ExitStatus { code })
    }

    async fn stop(&self, runner: &dyn CommandRunner, h: &SandboxHandle) -> Result<()> {
        let argv = self.argv(&["stop", check_id(&h.id)?]);
        self.run(runner, &argv, Some(self.not_found))
            .await
            .map(drop)
    }

    async fn destroy(
        &self,
        runner: &dyn CommandRunner,
        exits: &Exits,
        h: &SandboxHandle,
    ) -> Result<()> {
        exits.remove(&h.id);
        let rm = if self.apple { "delete" } else { "rm" };
        let argv = self.argv(&[rm, "--force", check_id(&h.id)?]);
        self.run(runner, &argv, Some(self.not_found))
            .await
            .map(drop)
    }
}

type SharedExit = Shared<BoxFuture<'static, std::result::Result<i32, Arc<std::io::Error>>>>;

/// Exit futures of the `start --attach` processes a driver ran, by sandbox id.
#[derive(Default)]
struct Exits(Mutex<HashMap<String, SharedExit>>);

impl Exits {
    fn map(&self) -> MutexGuard<'_, HashMap<String, SharedExit>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn insert(&self, id: &str, exit: ExitFuture) {
        let shared = exit.map(|r| r.map_err(Arc::new)).boxed().shared();
        self.map().insert(id.to_string(), shared);
    }

    fn get(&self, id: &str) -> Option<SharedExit> {
        self.map().get(id).cloned()
    }

    fn remove(&self, id: &str) {
        self.map().remove(id);
    }
}

/// Key suffixes (upper-cased) whose values are treated as secrets.
const SECRET_SUFFIXES: [&str; 5] = [
    "_API_KEY",
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "AWS_SECRET_ACCESS_KEY",
];

/// Rejects secret-named env vars (case-insensitive) with a real-looking value. Empty values
/// and masked placeholders of exactly the form `[SECRET_<digits>]` pass.
fn check_secrets(env: &BTreeMap<String, String>) -> Result<()> {
    let secret_key = |k: &str| {
        let k = k.to_ascii_uppercase();
        SECRET_SUFFIXES.iter().any(|s| k.ends_with(s))
    };
    let placeholder = |v: &str| {
        v.strip_prefix("[SECRET_")
            .and_then(|r| r.strip_suffix(']'))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    match env
        .iter()
        .find(|(k, v)| secret_key(k) && !v.is_empty() && !placeholder(v))
    {
        Some((key, _)) => Err(SandboxError::SecretInEnv { key: key.clone() }),
        None => Ok(()),
    }
}

fn check_image(image: &str) -> Result<()> {
    if image.is_empty() || image.starts_with('-') {
        return Err(SandboxError::InvalidImage {
            image: image.to_string(),
        });
    }
    Ok(())
}

/// Checks that `path` can be mounted as a workspace: absolute, UTF-8, and free of `,` `=` `"`
/// and newlines, which would change the `--mount` options. Returns the path as text.
/// Drivers run this in `create`; callers that make workspaces can run it up front.
pub fn validate_workspace_path(path: &Path) -> Result<&str> {
    match path.to_str() {
        Some(text) if path.is_absolute() && !text.contains([',', '=', '"', '\n', '\r']) => Ok(text),
        _ => Err(SandboxError::InvalidWorkspace {
            path: path.display().to_string(),
        }),
    }
}

fn check_id(id: &str) -> Result<&str> {
    let ok = !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if ok {
        return Ok(id);
    }
    Err(SandboxError::InvalidId { id: id.to_string() })
}

macro_rules! cli_driver {
    ($ty:ident, $cli:ident) => {
        impl $ty {
            pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
                Self {
                    runner,
                    exits: Exits::default(),
                }
            }
        }

        #[async_trait]
        impl SandboxDriver for $ty {
            fn name(&self) -> &'static str {
                $cli.name
            }
            fn is_strong(&self) -> bool {
                $cli.strong
            }
            async fn create(&self, spec: &LaunchSpec) -> Result<SandboxHandle> {
                $cli.create(self.runner.as_ref(), spec).await
            }
            async fn start(&self, h: &SandboxHandle) -> Result<OutputStream> {
                $cli.start(self.runner.as_ref(), &self.exits, h).await
            }
            async fn wait(&self, h: &SandboxHandle) -> Result<ExitStatus> {
                $cli.wait(self.runner.as_ref(), &self.exits, h).await
            }
            async fn stop(&self, h: &SandboxHandle) -> Result<()> {
                $cli.stop(self.runner.as_ref(), h).await
            }
            async fn destroy(&self, h: &SandboxHandle) -> Result<()> {
                $cli.destroy(self.runner.as_ref(), &self.exits, h).await
            }
        }
    };
}

/// `docker create --runtime=runsc`.
pub struct GvisorDriver {
    runner: Arc<dyn CommandRunner>,
    exits: Exits,
}

/// Apple `container` CLI (macOS 26+, Apple Silicon).
///
/// `wait` reports the exit of the `start --attach` process, so keep polling the stream from
/// `start` while waiting: a full pipe blocks the container, and dropping the stream early can
/// make the exit report -1.
pub struct AppleContainerDriver {
    runner: Arc<dyn CommandRunner>,
    exits: Exits,
}

/// Plain docker with runc. Weak isolation, trusted runtimes only.
pub struct DockerRuncDriver {
    runner: Arc<dyn CommandRunner>,
    exits: Exits,
}

cli_driver!(GvisorDriver, GVISOR);
cli_driver!(AppleContainerDriver, APPLE);
cli_driver!(DockerRuncDriver, DOCKER_RUNC);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    /// `major` is the macOS major version, for example 26.
    MacOs {
        major: u32,
    },
    Windows,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
    Other,
}

/// What the host has, as probed once at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    pub os: Os,
    pub arch: Arch,
    /// Runtimes registered with docker (`docker info`), for example `["runc", "runsc"]`.
    /// Empty when docker is not installed.
    pub docker_runtimes: Vec<String>,
    /// Whether the Apple `container` CLI is installed.
    pub apple_container: bool,
}

const INSTALL: &str = "Linux: install gVisor and register `runsc` with docker \
(https://gvisor.dev/docs/user_guide/install/). macOS 26+ on Apple Silicon: install Apple \
`container` (https://github.com/apple/container). To accept weak isolation for trusted \
runtimes only, allow docker-runc.";

/// Usable drivers for `host`, strongest first. Drivers run real processes via [`ProcessRunner`].
pub fn detect(host: &HostInfo, allow_weak: bool) -> Result<Vec<Box<dyn SandboxDriver>>> {
    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessRunner);
    let has_docker_runtime = |name: &str| host.docker_runtimes.iter().any(|r| r == name);
    let mut drivers: Vec<Box<dyn SandboxDriver>> = Vec::new();
    if host.os == Os::Linux && has_docker_runtime("runsc") {
        drivers.push(Box::new(GvisorDriver::new(runner.clone())));
    }
    if matches!(host.os, Os::MacOs { major } if major >= 26)
        && host.arch == Arch::Aarch64
        && host.apple_container
    {
        drivers.push(Box::new(AppleContainerDriver::new(runner.clone())));
    }
    if allow_weak && has_docker_runtime("runc") {
        drivers.push(Box::new(DockerRuncDriver::new(runner)));
    }
    if drivers.is_empty() {
        return Err(SandboxError::NoSecureSandbox {
            instructions: INSTALL.to_string(),
        });
    }
    Ok(drivers)
}
