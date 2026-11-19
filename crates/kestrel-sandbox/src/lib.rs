//! Sandbox drivers: gVisor on Linux, Apple container on macOS.
//!
//! Every process a driver starts goes through a [`CommandRunner`], so unit tests swap in a fake
//! and never spawn a real process.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use std::process::Stdio;

use async_trait::async_trait;
use futures::Stream;
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
    /// Start and stream stdout and stderr lines as they arrive.
    async fn spawn(&self, argv: &[String]) -> std::io::Result<OutputStream>;
}

/// The real runner: spawns host processes.
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

    async fn spawn(&self, argv: &[String]) -> std::io::Result<OutputStream> {
        let mut child = command(argv)?
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(std::io::Error::other("child has no stdout or stderr pipe"));
        };
        // Reap the child in the background so it never lingers as a zombie.
        tokio::spawn(async move { child.wait().await });
        Ok(Box::pin(futures::stream::select(
            lines(out, StdStream::Stdout),
            lines(err, StdStream::Stderr),
        )))
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
        if let NetworkPolicy::GatewayOnly { .. } = spec.network {
            let argv = self.argv(&["network", "create", "--internal", GATEWAY_NETWORK]);
            self.run(runner, &argv, Some("already exists")).await?;
        }
        let stdout = self.run(runner, &self.create_argv(spec), None).await?;
        Ok(SandboxHandle {
            id: stdout.trim().to_string(),
        })
    }

    async fn start(&self, runner: &dyn CommandRunner, h: &SandboxHandle) -> Result<OutputStream> {
        let argv = self.argv(&["start", "--attach", &h.id]);
        runner
            .spawn(&argv)
            .await
            .map_err(|source| SandboxError::Spawn {
                program: self.program.to_string(),
                source,
            })
    }

    async fn wait(&self, runner: &dyn CommandRunner, h: &SandboxHandle) -> Result<ExitStatus> {
        if self.apple {
            // The Apple CLI has no `wait` command and does not report exit codes after the fact.
            return Err(SandboxError::Spawn {
                program: "container wait".to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "the Apple container CLI cannot report a container's exit code",
                ),
            });
        }
        let stdout = self.run(runner, &self.argv(&["wait", &h.id]), None).await?;
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
        let argv = self.argv(&["stop", &h.id]);
        self.run(runner, &argv, Some(self.not_found))
            .await
            .map(drop)
    }

    async fn destroy(&self, runner: &dyn CommandRunner, h: &SandboxHandle) -> Result<()> {
        let rm = if self.apple { "delete" } else { "rm" };
        let argv = self.argv(&[rm, "--force", &h.id]);
        self.run(runner, &argv, Some(self.not_found))
            .await
            .map(drop)
    }
}

/// Rejects `*_API_KEY` vars with a real-looking value. Empty values and masked
/// placeholders such as `[SECRET_3]` pass.
fn check_secrets(env: &BTreeMap<String, String>) -> Result<()> {
    let real = |v: &str| !v.is_empty() && !(v.starts_with('[') && v.ends_with(']'));
    match env.iter().find(|(k, v)| k.ends_with("_API_KEY") && real(v)) {
        Some((key, _)) => Err(SandboxError::SecretInEnv { key: key.clone() }),
        None => Ok(()),
    }
}

macro_rules! cli_driver {
    ($ty:ident, $cli:ident) => {
        impl $ty {
            pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
                Self { runner }
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
                $cli.start(self.runner.as_ref(), h).await
            }
            async fn wait(&self, h: &SandboxHandle) -> Result<ExitStatus> {
                $cli.wait(self.runner.as_ref(), h).await
            }
            async fn stop(&self, h: &SandboxHandle) -> Result<()> {
                $cli.stop(self.runner.as_ref(), h).await
            }
            async fn destroy(&self, h: &SandboxHandle) -> Result<()> {
                $cli.destroy(self.runner.as_ref(), h).await
            }
        }
    };
}

/// `docker create --runtime=runsc`.
pub struct GvisorDriver {
    runner: Arc<dyn CommandRunner>,
}

/// Apple `container` CLI (macOS 26+, Apple Silicon).
pub struct AppleContainerDriver {
    runner: Arc<dyn CommandRunner>,
}

/// Plain docker with runc. Weak isolation, trusted runtimes only.
pub struct DockerRuncDriver {
    runner: Arc<dyn CommandRunner>,
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
