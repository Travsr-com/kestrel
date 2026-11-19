//! Sandbox drivers: gVisor on Linux, Apple container on macOS.
//!
//! Every process a driver starts goes through a [`CommandRunner`], so unit tests swap in a fake
//! and never spawn a real process.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
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
        todo!()
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
    async fn run(&self, _argv: &[String]) -> std::io::Result<CommandOutput> {
        todo!()
    }
    async fn spawn(&self, _argv: &[String]) -> std::io::Result<OutputStream> {
        todo!()
    }
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

macro_rules! skeleton_driver {
    ($ty:ident) => {
        impl $ty {
            pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
                Self { runner }
            }
        }

        #[async_trait]
        impl SandboxDriver for $ty {
            fn name(&self) -> &'static str {
                todo!()
            }
            fn is_strong(&self) -> bool {
                todo!()
            }
            async fn create(&self, _spec: &LaunchSpec) -> Result<SandboxHandle> {
                todo!()
            }
            async fn start(&self, _h: &SandboxHandle) -> Result<OutputStream> {
                todo!()
            }
            async fn wait(&self, _h: &SandboxHandle) -> Result<ExitStatus> {
                todo!()
            }
            async fn stop(&self, _h: &SandboxHandle) -> Result<()> {
                todo!()
            }
            async fn destroy(&self, _h: &SandboxHandle) -> Result<()> {
                todo!()
            }
        }
    };
}

/// `docker create --runtime=runsc`.
pub struct GvisorDriver {
    #[allow(dead_code)]
    runner: Arc<dyn CommandRunner>,
}

/// Apple `container` CLI (macOS 26+, Apple Silicon).
pub struct AppleContainerDriver {
    #[allow(dead_code)]
    runner: Arc<dyn CommandRunner>,
}

/// Plain docker with runc. Weak isolation, trusted runtimes only.
pub struct DockerRuncDriver {
    #[allow(dead_code)]
    runner: Arc<dyn CommandRunner>,
}

skeleton_driver!(GvisorDriver);
skeleton_driver!(AppleContainerDriver);
skeleton_driver!(DockerRuncDriver);

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

/// Usable drivers for `host`, strongest first.
pub fn detect(_host: &HostInfo, _allow_weak: bool) -> Result<Vec<Box<dyn SandboxDriver>>> {
    todo!()
}
