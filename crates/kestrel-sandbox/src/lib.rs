//! Sandbox drivers: gVisor on Linux, Apple container on macOS.
//!
//! Every process a driver starts goes through a [`CommandRunner`], so unit tests swap in a fake
//! and never spawn a real process.

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use std::process::Stdio;

use async_trait::async_trait;
use futures::future::{BoxFuture, Shared};
use futures::{FutureExt, Stream};
use kestrel_types::{GATEWAY_KEY_PLACEHOLDER, RunId};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use url::Url;

mod bridge;
mod roots;
mod subnet;
use bridge::is_container_name;
pub use bridge::{BRIDGE_NAME, BridgeSpec, DEFAULT_BRIDGE_IMAGE, GatewayBridge};
pub use roots::{RootsError, resolve_workspace};
pub use subnet::{
    DEFAULT_SUBNET_POOL, Ipv4Cidr, SUBNET_BLOCK_PREFIX, Subnet, SubnetError, SubnetPool,
    docker_subnets, docker_subnets_except_instance, host_routes,
};

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
    /// A config directory path that is not absolute UTF-8, or that would change the `--mount`
    /// options.
    #[error(
        "invalid config directory `{path}`: must be absolute UTF-8 without `,` `=` `\"` or newlines"
    )]
    InvalidConfig { path: String },
    /// A container id that is empty, starts with `-`, or has characters outside
    /// `[A-Za-z0-9_.-]`.
    #[error("invalid container id `{id}`")]
    InvalidId { id: String },
    /// A bridge step failed. `step` is `remove-stale`, `create-network`, `run`, `connect` or `inspect`.
    #[error("gateway bridge step `{step}` failed: {stderr}")]
    Bridge { step: &'static str, stderr: String },
    /// The bridge port is 0.
    #[error("invalid bridge port 0")]
    InvalidBridgePort,
    /// Docker refused a run network's block because another network already uses it ("Pool
    /// overlaps"). The engine excludes the block and tries another one.
    #[error("subnet {subnet} is taken by another docker network")]
    SubnetTaken { subnet: Ipv4Cidr },
    /// A step of making a run network failed. `step` is `create`, `connect` or `inspect`.
    #[error("run network `{network}`: step `{step}` failed: {stderr}")]
    RunNetwork {
        step: &'static str,
        network: String,
        stderr: String,
    },
    /// The workspace directory could not be inspected (missing, no permission, ...), so the
    /// host user to run the sandbox as is unknown.
    #[error("cannot read the owner of workspace `{path}`: {source}")]
    WorkspaceUnreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The workspace is not inside an allowed root (or cannot be resolved).
    #[error(transparent)]
    Roots(RootsError),
}

impl From<RootsError> for SandboxError {
    fn from(e: RootsError) -> Self {
        SandboxError::Roots(e)
    }
}

pub type Result<T> = std::result::Result<T, SandboxError>;

/// Network access for a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No network at all.
    None,
    /// Only the Kestrel gateway is reachable, through the shared internal network `kestrel-gw`.
    /// Sandboxes on it can reach each other, so the engine uses it only for drivers without run
    /// networks (no bridge, that is `gateway.advertise_url` set, or Apple `container`).
    GatewayOnly { gateway: Url },
    /// Only the Kestrel gateway is reachable, through the run's own internal network made by
    /// [`SandboxDriver::prepare_network`]. The sandbox is attached to that network only.
    RunNetwork { network: String, gateway: Url },
}

/// A run's own network, made by [`SandboxDriver::prepare_network`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedNetwork {
    /// Docker network name, `kestrel-gw-<last 12 hex chars of the run id>`.
    pub name: String,
    /// The bridge's IP on this network: the gateway host of the run's launch plan.
    pub gateway_host: Ipv4Addr,
    pub subnet: Subnet,
}

/// Returns the `(uid, gid)` that owns a workspace directory. Injected into drivers so tests do
/// not need root; the default reads the directory's real owner.
pub type OwnerLookup = Arc<dyn Fn(&Path) -> std::io::Result<(u32, u32)> + Send + Sync>;

/// The real lookup: the owner of `dir` on disk (unix only).
#[cfg(unix)]
fn host_owner(dir: &Path) -> std::io::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir)?;
    Ok((meta.uid(), meta.gid()))
}

/// No host user to map to off unix: report root, which adds no `--user`.
#[cfg(not(unix))]
fn host_owner(_dir: &Path) -> std::io::Result<(u32, u32)> {
    Ok((0, 0))
}

fn default_owner() -> OwnerLookup {
    Arc::new(host_owner)
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
    /// Host directory mounted read-only at `/kestrel`: the run's config (its launch plan's files),
    /// outside the workspace so the agent can neither change nor commit it.
    pub config: Option<PathBuf>,
    pub cpu_millis: u32,
    pub memory_mb: u64,
    pub network: NetworkPolicy,
    /// Labels set on the container (`--label key=value`), so a later sweep can find containers
    /// a crashed engine left behind. The engine sets [`RUN_LABEL`] to the run id.
    pub labels: BTreeMap<String, String>,
}

/// Label key every sandbox of a run carries; the value is the run id.
pub const RUN_LABEL: &str = "kestrel.run";

/// Label key that names the Kestrel instance a sandbox belongs to, when the engine has an
/// instance id. A driver with the same instance id lists only those sandboxes, so two instances
/// on one Docker host do not remove each other's.
pub const INSTANCE_LABEL: &str = "kestrel.instance";

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

/// Environment changes for one process, applied on top of the inherited environment.
/// A `remove` entry ending in `*` is a prefix (`GIT_SSH*` removes `GIT_SSH` and `GIT_SSH_COMMAND`).
/// `remove` is applied first, then `set`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvScrub {
    pub remove: Vec<String>,
    pub set: Vec<(String, String)>,
}

/// All process spawning goes through this. `argv[0]` is the program; nothing is run via a shell.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run to completion and capture output.
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput>;
    /// Like [`run`](Self::run), with `env` applied to the child's environment. The default
    /// ignores `env` so existing implementors (fakes) keep working; `ProcessRunner` must override it.
    async fn run_with_env(
        &self,
        argv: &[String],
        env: &EnvScrub,
    ) -> std::io::Result<CommandOutput> {
        let _ = env;
        self.run(argv).await
    }
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
        output_of(command(argv)?).await
    }

    async fn run_with_env(
        &self,
        argv: &[String],
        env: &EnvScrub,
    ) -> std::io::Result<CommandOutput> {
        let mut cmd = command(argv)?;
        env.apply(&mut cmd);
        output_of(cmd).await
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

impl EnvScrub {
    /// Applies `remove` (exact names, or prefixes ending in `*`), then `set`, to `cmd`.
    fn apply(&self, cmd: &mut tokio::process::Command) {
        for pattern in &self.remove {
            match pattern.strip_suffix('*') {
                Some(prefix) => {
                    for (name, _) in std::env::vars_os() {
                        if name.to_string_lossy().starts_with(prefix) {
                            cmd.env_remove(name);
                        }
                    }
                }
                None => {
                    cmd.env_remove(pattern);
                }
            }
        }
        cmd.envs(self.set.iter().map(|(k, v)| (k, v)));
    }
}

/// Runs `cmd` to completion and captures its output.
async fn output_of(mut cmd: tokio::process::Command) -> std::io::Result<CommandOutput> {
    let out = cmd.output().await?;
    Ok(CommandOutput {
        // A process killed by a signal has no code.
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
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
    /// Sandboxes carrying the [`RUN_LABEL`] label, running or not, whether or not this process
    /// created them. A driver given an instance id (`with_instance`) lists only the sandboxes
    /// labelled with [`INSTANCE_LABEL`] set to that id. Drivers that cannot list by label return
    /// nothing (the default).
    async fn leftovers(&self) -> Result<Vec<SandboxHandle>> {
        Ok(Vec::new())
    }
    /// Creates `run`'s own internal network on `subnet` and joins the gateway bridge to it.
    /// `None` when the driver has no run networks (the default): the run then uses
    /// [`NetworkPolicy::GatewayOnly`]. A docker answer that the block is in use is
    /// [`SandboxError::SubnetTaken`]; any other failure removes what was made and names the step.
    /// A network of the run's own name that is already there (left by a prepare that was cut
    /// short) is removed and made again, once.
    async fn prepare_network(
        &self,
        run: RunId,
        subnet: &Subnet,
    ) -> Result<Option<PreparedNetwork>> {
        let _ = (run, subnet);
        Ok(None)
    }
    /// Disconnects the bridge from `network` and removes it. Idempotent: a network or a bridge
    /// that is already gone is fine. The default does nothing.
    async fn release_network(&self, network: &PreparedNetwork) -> Result<()> {
        let _ = network;
        Ok(())
    }
    /// Removes every leftover run network (labelled [`RUN_LABEL`], and [`INSTANCE_LABEL`] set to
    /// the driver's instance id when it has one), whether or not this process made it. Networks
    /// without the label are never touched. The default does nothing.
    async fn sweep_networks(&self) -> Result<()> {
        Ok(())
    }
    /// The IPv4 subnets other networks use right now, leaving out the run networks of this
    /// driver's instance (see [`docker_subnets_except_instance`]). The engine excludes them all
    /// from its pool after one [`SandboxError::SubnetTaken`], instead of finding them one refused
    /// block at a time. The default knows of none.
    async fn taken_subnets(&self) -> Result<Vec<Ipv4Cidr>> {
        Ok(Vec::new())
    }
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
        self.run_tolerating(runner, argv, tolerated.as_slice())
            .await
    }

    /// Runs `argv`. A non-zero exit whose stderr contains any of `tolerated` counts as success.
    async fn run_tolerating(
        &self,
        runner: &dyn CommandRunner,
        argv: &[String],
        tolerated: &[&str],
    ) -> Result<String> {
        let out = runner
            .run(argv)
            .await
            .map_err(|source| SandboxError::Spawn {
                program: self.program.to_string(),
                source,
            })?;
        if out.status == 0 || tolerated.iter().any(|t| out.stderr.contains(t)) {
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

    fn create_argv(&self, spec: &LaunchSpec, user: Option<(u32, u32)>) -> Vec<String> {
        let suffix = if self.apple { "M" } else { "m" };
        let network = match &spec.network {
            NetworkPolicy::None => "none",
            NetworkPolicy::GatewayOnly { .. } => GATEWAY_NETWORK,
            NetworkPolicy::RunNetwork { network, .. } => network.as_str(),
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
        if let Some((uid, gid)) = user {
            argv.extend(["--user".to_string(), format!("{uid}:{gid}")]);
        }
        if let Some(ws) = &spec.workspace {
            argv.extend([
                "--mount".to_string(),
                format!("type=bind,source={},target=/workspace", ws.display()),
                "--workdir".to_string(),
                "/workspace".to_string(),
            ]);
        }
        if let Some(config) = &spec.config {
            argv.extend([
                "--mount".to_string(),
                format!(
                    "type=bind,source={},target=/kestrel,readonly",
                    config.display()
                ),
            ]);
        }
        let gateway = match &spec.network {
            NetworkPolicy::None => None,
            NetworkPolicy::GatewayOnly { gateway } | NetworkPolicy::RunNetwork { gateway, .. } => {
                Some(("KESTREL_GATEWAY_URL", gateway.as_str()))
            }
        };
        let mut env: Vec<(&str, &str)> = spec
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if user.is_some() && !spec.env.contains_key("HOME") {
            // A non-root user may have no writable home in the image.
            env.push(("HOME", "/tmp"));
            env.sort();
        }
        for (k, v) in env.into_iter().chain(gateway) {
            argv.extend(["--env".to_string(), format!("{k}={v}")]);
        }
        for (key, value) in &spec.labels {
            argv.extend(["--label".to_string(), format!("{key}={value}")]);
        }
        argv.push(spec.image.clone());
        argv.extend(spec.argv.iter().cloned());
        argv
    }

    /// Ids of the containers labelled [`RUN_LABEL`] (and [`INSTANCE_LABEL`] when `instance` is
    /// given). Ids that are not safe to pass to the CLI again are dropped. Apple `container` has
    /// no label filter, so it lists nothing and runs no process.
    async fn leftovers(
        &self,
        runner: &dyn CommandRunner,
        instance: Option<&str>,
    ) -> Result<Vec<SandboxHandle>> {
        if self.apple {
            return Ok(Vec::new());
        }
        let run_filter = format!("label={RUN_LABEL}");
        let mut args = vec!["ps", "-a", "--filter", run_filter.as_str()];
        let instance_filter = match instance {
            Some(id) => Some(format!("label={INSTANCE_LABEL}={}", check_id(id)?)),
            None => None,
        };
        args.extend(
            instance_filter
                .iter()
                .flat_map(|f| ["--filter", f.as_str()]),
        );
        args.extend(["--format", "{{.ID}}"]);
        let stdout = self.run(runner, &self.argv(&args), None).await?;
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|id| check_id(id).is_ok())
            .map(|id| SandboxHandle { id: id.to_string() })
            .collect())
    }

    async fn create(
        &self,
        runner: &dyn CommandRunner,
        owner: &OwnerLookup,
        roots: Option<&[PathBuf]>,
        spec: &LaunchSpec,
    ) -> Result<SandboxHandle> {
        check_secrets(&spec.env)?;
        check_image(&spec.image)?;
        if let Some(config) = &spec.config {
            validate_workspace_path(config).map_err(|_| SandboxError::InvalidConfig {
                path: config.display().to_string(),
            })?;
        }
        let mut user = None;
        let mut resolved;
        let mut spec = spec;
        if let (Some(roots), Some(ws)) = (roots, &spec.workspace) {
            validate_workspace_path(ws)?;
            let canonical = resolve_workspace(ws, roots)?;
            validate_workspace_path(&canonical)?;
            resolved = spec.clone();
            resolved.workspace = Some(canonical);
            spec = &resolved;
        }
        if let Some(ws) = &spec.workspace {
            validate_workspace_path(ws)?;
            let ids = owner(ws).map_err(|source| SandboxError::WorkspaceUnreadable {
                path: ws.display().to_string(),
                source,
            })?;
            // Root-owned workspace: Kestrel runs as root, nothing to map.
            user = (ids != (0, 0)).then_some(ids);
        }
        match &spec.network {
            NetworkPolicy::GatewayOnly { .. } => {
                let argv = self.argv(&["network", "create", "--internal", GATEWAY_NETWORK]);
                self.run(runner, &argv, Some("already exists")).await?;
            }
            NetworkPolicy::RunNetwork { network, .. } => {
                check_network_name(network)?;
            }
            NetworkPolicy::None => {}
        }
        let stdout = self
            .run(runner, &self.create_argv(spec, user), None)
            .await?;
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

    /// See [`SandboxDriver::prepare_network`]. `bridge` is the bridge container name; without
    /// one, and for Apple `container`, there are no run networks. A network of the run's name
    /// that already exists and carries this run's labels was left by a prepare that was cut
    /// short: it is removed and the create runs once more. One without them is left alone.
    async fn prepare_network(
        &self,
        runner: &dyn CommandRunner,
        net: NetworkContext<'_>,
        run: RunId,
        subnet: &Subnet,
    ) -> Result<Option<PreparedNetwork>> {
        let Some(bridge) = net.bridge.filter(|_| !self.apple) else {
            return Ok(None);
        };
        check_network_name(bridge)?;
        let name = run_network_name(run);
        let cidr = subnet.cidr().to_string();
        let gateway = subnet.gateway().to_string();
        let run_label = format!("{RUN_LABEL}={run}");
        let instance_label = match net.instance {
            Some(id) => Some(format!("{INSTANCE_LABEL}={}", check_id(id)?)),
            None => None,
        };
        let mut args = vec![
            "network",
            "create",
            "--internal",
            // No host address on the network's bridge, so a sandbox cannot reach host services
            // through docker's gateway address. The bridge container is the only way out.
            "-o",
            "com.docker.network.bridge.inhibit_ipv4=true",
            "--subnet",
            &cidr,
            "--gateway",
            &gateway,
            "--label",
            &run_label,
        ];
        args.extend(instance_label.iter().flat_map(|l| ["--label", l.as_str()]));
        args.push(&name);
        let step = |step: &'static str| {
            let network = name.clone();
            move |e: SandboxError| SandboxError::RunNetwork {
                step,
                network,
                stderr: match e {
                    SandboxError::CommandFailed { stderr, .. } => stderr,
                    other => other.to_string(),
                },
            }
        };
        let create = self.argv(&args);
        let mut created = self.run(runner, &create, None).await;
        if let Err(SandboxError::CommandFailed { stderr, .. }) = &created
            && is_left_behind(stderr, &name)
            && self
                .is_this_runs(runner, &name, &run_label, instance_label.as_deref())
                .await
        {
            // A prepare cut short (its actor aborted) left this run's network behind, unrecorded.
            // Docker confirmed it is this run's own, so it goes, and the create runs once more.
            let removed = self.remove_network(runner, Some(bridge), &name).await;
            created = self.run(runner, &create, None).await;
            if let (Err(SandboxError::CommandFailed { stderr, .. }), Err(rm)) =
                (&mut created, removed)
            {
                let rm = match rm {
                    SandboxError::CommandFailed { stderr, .. } => stderr,
                    other => other.to_string(),
                };
                stderr.push_str(&format!(
                    "; removing the network left behind failed first: {rm}"
                ));
            }
        }
        match created {
            Err(SandboxError::CommandFailed { stderr, .. }) if stderr.contains("Pool overlaps") => {
                return Err(SandboxError::SubnetTaken {
                    subnet: subnet.cidr(),
                });
            }
            other => other.map_err(step("create"))?,
        };
        let joined = async {
            let connect = self.argv(&["network", "connect", &name, bridge]);
            self.run(runner, &connect, None)
                .await
                .map_err(step("connect"))?;
            let format = format!("{{{{(index .NetworkSettings.Networks \"{name}\").IPAddress}}}}");
            // `container inspect`: a network or image named like the bridge cannot match.
            let inspect = self.argv(&["container", "inspect", "--format", &format, bridge]);
            let answer = self
                .run(runner, &inspect, None)
                .await
                .map_err(step("inspect"))?;
            answer.trim().parse::<Ipv4Addr>().map_err(|_| {
                step("inspect")(SandboxError::CommandFailed {
                    program: self.program.to_string(),
                    code: 0,
                    stderr: format!("the bridge has no single IPv4 address on {name}: {answer:?}"),
                })
            })
        }
        .await;
        match joined {
            Ok(gateway_host) => Ok(Some(PreparedNetwork {
                name,
                gateway_host,
                subnet: subnet.clone(),
            })),
            Err(e) => {
                // Best effort: the failed step is the error worth reporting.
                let _ = self.remove_network(runner, Some(bridge), &name).await;
                Err(e)
            }
        }
    }

    /// Whether docker lists network `name` with label `run_label` (`kestrel.run=<run id>`) and,
    /// when given, `instance_label` (`kestrel.instance=<id>`): a network of this run's name that
    /// is this run's own. A listing that fails confirms nothing.
    async fn is_this_runs(
        &self,
        runner: &dyn CommandRunner,
        name: &str,
        run_label: &str,
        instance_label: Option<&str>,
    ) -> bool {
        let name_filter = format!("name=^{name}$");
        let run_filter = format!("label={run_label}");
        let instance_filter = instance_label.map(|l| format!("label={l}"));
        let mut args = vec![
            "network",
            "ls",
            "-q",
            "--filter",
            &name_filter,
            "--filter",
            &run_filter,
        ];
        args.extend(
            instance_filter
                .iter()
                .flat_map(|f| ["--filter", f.as_str()]),
        );
        match self.run(runner, &self.argv(&args), None).await {
            Ok(ids) => ids.lines().any(|id| !id.trim().is_empty()),
            Err(_) => false,
        }
    }

    /// Disconnects `bridge` (when given) from network `name`, then removes the network. A
    /// network or connection that is already gone is fine. Both steps always run. The result is
    /// the `network rm` one: once the network is gone its block is free, whatever the disconnect
    /// said (for example that the bridge container is gone); if `rm` fails, that is the error.
    async fn remove_network(
        &self,
        runner: &dyn CommandRunner,
        bridge: Option<&str>,
        name: &str,
    ) -> Result<()> {
        if self.apple {
            return Ok(());
        }
        check_network_name(name)?;
        if let Some(bridge) = bridge {
            let disconnect = self.argv(&["network", "disconnect", "--force", name, bridge]);
            // A disconnect that failed for a reason that matters also makes `rm` fail.
            let _ = self
                .run_tolerating(runner, &disconnect, &NETWORK_GONE)
                .await;
        }
        let rm = self.argv(&["network", "rm", name]);
        self.run_tolerating(runner, &rm, &NETWORK_GONE)
            .await
            .map(drop)
    }

    /// See [`SandboxDriver::taken_subnets`]. Apple `container` has no run networks: nothing.
    async fn taken_subnets(
        &self,
        runner: &dyn CommandRunner,
        instance: Option<&str>,
    ) -> Result<Vec<Ipv4Cidr>> {
        if self.apple {
            return Ok(Vec::new());
        }
        docker_subnets_except_instance(runner, instance).await
    }

    /// See [`SandboxDriver::sweep_networks`]. Without a bridge the networks are only removed.
    async fn sweep_networks(
        &self,
        runner: &dyn CommandRunner,
        net: NetworkContext<'_>,
    ) -> Result<()> {
        if self.apple {
            return Ok(());
        }
        let run_filter = format!("label={RUN_LABEL}");
        let mut args = vec!["network", "ls", "--filter", run_filter.as_str()];
        let instance_filter = match net.instance {
            Some(id) => Some(format!("label={INSTANCE_LABEL}={}", check_id(id)?)),
            None => None,
        };
        args.extend(
            instance_filter
                .iter()
                .flat_map(|f| ["--filter", f.as_str()]),
        );
        args.extend(["--format", "{{.Name}}"]);
        let names = self.run(runner, &self.argv(&args), None).await?;
        let mut first = Ok(());
        // Names are data from docker: one that is not a plain name never reaches the CLI.
        for name in names
            .lines()
            .map(str::trim)
            .filter(|n| is_container_name(n))
        {
            let removed = self.remove_network(runner, net.bridge, name).await;
            if first.is_ok() {
                first = removed;
            }
        }
        first
    }
}

/// Docker's answers for a network, or a connection to one, that is already gone. Shared with the
/// subnet plan, where a network removed between `ls` and `inspect` is skipped.
const NETWORK_GONE: [&str; 3] = ["not found", "No such network", "is not connected"];

/// Whether `stderr` is docker's answer that network `name` itself already exists ("network with
/// name <name> already exists"), not some other object.
fn is_left_behind(stderr: &str, name: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    stderr.contains("already exists") && stderr.split(|c: char| !word(c)).any(|w| w == name)
}

/// What a driver knows about networks: its gateway bridge container and its instance id.
#[derive(Clone, Copy)]
struct NetworkContext<'a> {
    bridge: Option<&'a str>,
    instance: Option<&'a str>,
}

/// `kestrel-gw-<last 12 hex chars of the run id>`.
fn run_network_name(run: RunId) -> String {
    let id = run.to_string();
    let tail = id.get(id.len().saturating_sub(12)..).unwrap_or(&id);
    format!("{GATEWAY_NETWORK}-{tail}")
}

/// A network or container name passed to the CLI: docker's name charset, so it can never be
/// read as an option.
fn check_network_name(name: &str) -> Result<&str> {
    if is_container_name(name) {
        return Ok(name);
    }
    Err(SandboxError::InvalidId {
        id: name.to_string(),
    })
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
/// and placeholders pass: masked ones of exactly the form `[SECRET_<digits>]`, and exactly
/// [`GATEWAY_KEY_PLACEHOLDER`], which the adapters give agents instead of a provider key.
fn check_secrets(env: &BTreeMap<String, String>) -> Result<()> {
    let secret_key = |k: &str| {
        let k = k.to_ascii_uppercase();
        SECRET_SUFFIXES.iter().any(|s| k.ends_with(s))
    };
    let placeholder = |v: &str| {
        v == GATEWAY_KEY_PLACEHOLDER
            || v.strip_prefix("[SECRET_")
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
                    owner: default_owner(),
                    instance: None,
                    bridge: None,
                    roots: None,
                }
            }

            /// Gives every run its own internal network, joined to the gateway bridge container
            /// `name` (see [`SandboxDriver::prepare_network`]). Without it there are no run
            /// networks. Apple `container` ignores it.
            pub fn with_bridge(mut self, name: impl Into<String>) -> Self {
                self.bridge = Some(name.into());
                self
            }

            fn bridged(self, bridge: Option<&str>) -> Self {
                match bridge {
                    Some(name) => self.with_bridge(name),
                    None => self,
                }
            }

            fn network_context(&self) -> NetworkContext<'_> {
                NetworkContext {
                    bridge: self.bridge.as_deref(),
                    instance: self.instance.as_deref(),
                }
            }

            /// Only mounts workspaces that resolve (symlinks and `..` followed) inside one of
            /// `roots`, and mounts the resolved path. Without this call any absolute path is
            /// accepted, as before.
            pub fn with_allowed_roots(mut self, roots: Vec<PathBuf>) -> Self {
                self.roots = Some(roots);
                self
            }

            /// Limits [`SandboxDriver::leftovers`] to sandboxes labelled with this instance id
            /// ([`INSTANCE_LABEL`]). The id must be `[A-Za-z0-9_.-]`, else `leftovers` fails.
            pub fn with_instance(mut self, id: impl Into<String>) -> Self {
                self.instance = Some(id.into());
                self
            }

            fn tag_roots(self, roots: Option<Vec<PathBuf>>) -> Self {
                match roots {
                    Some(r) => self.with_allowed_roots(r),
                    None => self,
                }
            }

            fn tag(self, instance: Option<&str>) -> Self {
                match instance {
                    Some(id) => self.with_instance(id),
                    None => self,
                }
            }

            /// Replaces how the workspace owner is read. For tests that cannot be root.
            pub fn with_workspace_owner(
                mut self,
                lookup: impl Fn(&Path) -> std::io::Result<(u32, u32)> + Send + Sync + 'static,
            ) -> Self {
                self.owner = Arc::new(lookup);
                self
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
                $cli.create(
                    self.runner.as_ref(),
                    &self.owner,
                    self.roots.as_deref(),
                    spec,
                )
                .await
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
            async fn leftovers(&self) -> Result<Vec<SandboxHandle>> {
                $cli.leftovers(self.runner.as_ref(), self.instance.as_deref())
                    .await
            }
            async fn prepare_network(
                &self,
                run: RunId,
                subnet: &Subnet,
            ) -> Result<Option<PreparedNetwork>> {
                $cli.prepare_network(self.runner.as_ref(), self.network_context(), run, subnet)
                    .await
            }
            async fn release_network(&self, network: &PreparedNetwork) -> Result<()> {
                $cli.remove_network(self.runner.as_ref(), self.bridge.as_deref(), &network.name)
                    .await
            }
            async fn sweep_networks(&self) -> Result<()> {
                $cli.sweep_networks(self.runner.as_ref(), self.network_context())
                    .await
            }
            async fn taken_subnets(&self) -> Result<Vec<Ipv4Cidr>> {
                $cli.taken_subnets(self.runner.as_ref(), self.instance.as_deref())
                    .await
            }
        }
    };
}

/// `docker create --runtime=runsc`.
pub struct GvisorDriver {
    runner: Arc<dyn CommandRunner>,
    exits: Exits,
    owner: OwnerLookup,
    instance: Option<String>,
    bridge: Option<String>,
    roots: Option<Vec<PathBuf>>,
}

/// Apple `container` CLI (macOS 26+, Apple Silicon).
///
/// `wait` reports the exit of the `start --attach` process, so keep polling the stream from
/// `start` while waiting: a full pipe blocks the container, and dropping the stream early can
/// make the exit report -1.
pub struct AppleContainerDriver {
    runner: Arc<dyn CommandRunner>,
    exits: Exits,
    owner: OwnerLookup,
    instance: Option<String>,
    bridge: Option<String>,
    roots: Option<Vec<PathBuf>>,
}

/// Plain docker with runc. Weak isolation, trusted runtimes only.
pub struct DockerRuncDriver {
    runner: Arc<dyn CommandRunner>,
    exits: Exits,
    owner: OwnerLookup,
    instance: Option<String>,
    bridge: Option<String>,
    roots: Option<Vec<PathBuf>>,
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

impl HostInfo {
    /// Probes this host: its OS and CPU, the runtimes registered with docker, and whether the
    /// Apple `container` CLI is installed. Runs real commands via [`ProcessRunner`].
    pub async fn current() -> HostInfo {
        Self::probe(&ProcessRunner, std::env::consts::OS, std::env::consts::ARCH).await
    }

    /// Like [`HostInfo::current`], with commands run through `runner` and `os` and `arch`
    /// spelled as in [`std::env::consts`]. A command that fails counts as "not installed".
    pub async fn probe(runner: &dyn CommandRunner, os: &str, arch: &str) -> HostInfo {
        let ok = |argv: &[&str]| {
            let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
            async move {
                match runner.run(&argv).await {
                    Ok(out) if out.status == 0 => Some(out.stdout),
                    _ => None,
                }
            }
        };
        let os = match os {
            "linux" => Os::Linux,
            "macos" => {
                let version = ok(&["sw_vers", "-productVersion"])
                    .await
                    .unwrap_or_default();
                let major = version.trim().split('.').next().unwrap_or_default();
                Os::MacOs {
                    major: major.parse().unwrap_or(0),
                }
            }
            "windows" => Os::Windows,
            _ => Os::Other,
        };
        let arch = match arch {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            _ => Arch::Other,
        };
        let runtimes = ok(&[
            "docker",
            "info",
            "--format",
            "{{range $name, $_ := .Runtimes}}{{$name}} {{end}}",
        ])
        .await
        .unwrap_or_default();
        HostInfo {
            os,
            arch,
            docker_runtimes: runtimes.split_whitespace().map(String::from).collect(),
            apple_container: ok(&["container", "--version"]).await.is_some(),
        }
    }
}

const INSTALL: &str = "Linux: install gVisor and register `runsc` with docker \
(https://gvisor.dev/docs/user_guide/install/). macOS 26+ on Apple Silicon: install Apple \
`container` (https://github.com/apple/container). To accept weak isolation for trusted \
runtimes only, allow docker-runc.";

/// Usable drivers for `host`, strongest first. Drivers run real processes via [`ProcessRunner`].
pub fn detect(host: &HostInfo, allow_weak: bool) -> Result<Vec<Box<dyn SandboxDriver>>> {
    detect_for(host, allow_weak, None)
}

/// Like [`detect`], with every driver limited to the sandboxes of `instance` when listing
/// leftovers (see [`SandboxDriver::leftovers`]).
pub fn detect_for(
    host: &HostInfo,
    allow_weak: bool,
    instance: Option<&str>,
) -> Result<Vec<Box<dyn SandboxDriver>>> {
    detect_with(host, allow_weak, instance, None)
}

/// Like [`detect_for`], with the docker drivers joined to the gateway bridge container `bridge`
/// (see `with_bridge`), so every run gets its own network.
pub fn detect_with(
    host: &HostInfo,
    allow_weak: bool,
    instance: Option<&str>,
    bridge: Option<&str>,
) -> Result<Vec<Box<dyn SandboxDriver>>> {
    detect_with_roots(host, allow_weak, instance, bridge, None)
}

/// Like [`detect_with`], and every driver only mounts workspaces inside `roots` (see
/// `with_allowed_roots`). `None` keeps the old behaviour.
pub fn detect_with_roots(
    host: &HostInfo,
    allow_weak: bool,
    instance: Option<&str>,
    bridge: Option<&str>,
    roots: Option<Vec<PathBuf>>,
) -> Result<Vec<Box<dyn SandboxDriver>>> {
    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessRunner);
    let has_docker_runtime = |name: &str| host.docker_runtimes.iter().any(|r| r == name);
    let mut drivers: Vec<Box<dyn SandboxDriver>> = Vec::new();
    if host.os == Os::Linux && has_docker_runtime("runsc") {
        drivers.push(Box::new(
            GvisorDriver::new(runner.clone())
                .tag(instance)
                .bridged(bridge)
                .tag_roots(roots.clone()),
        ));
    }
    if matches!(host.os, Os::MacOs { major } if major >= 26)
        && host.arch == Arch::Aarch64
        && host.apple_container
    {
        // Apple `container` has no run networks; the bridge is ignored there.
        drivers.push(Box::new(
            AppleContainerDriver::new(runner.clone())
                .tag(instance)
                .bridged(bridge)
                .tag_roots(roots.clone()),
        ));
    }
    if allow_weak && has_docker_runtime("runc") {
        drivers.push(Box::new(
            DockerRuncDriver::new(runner)
                .tag(instance)
                .bridged(bridge)
                .tag_roots(roots),
        ));
    }
    if drivers.is_empty() {
        return Err(SandboxError::NoSecureSandbox {
            instructions: INSTALL.to_string(),
        });
    }
    Ok(drivers)
}
