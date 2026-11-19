//! Scheduler, run state machine, budgets and approvals.
//!
//! The [`Engine`] drives a run from `Queued` to a terminal status: it provisions a sandbox, launches
//! the agent through its runtime adapter, streams the agent's events into the [`Store`], enforces
//! budgets and the wall clock, and applies control commands (pause, resume, cancel, approve, reject,
//! set budget) read from [`Store::take_control`].

use std::sync::Arc;
use std::time::Duration;

use kestrel_agent::RuntimeAdapter;
use kestrel_sandbox::SandboxDriver;
use kestrel_store::{Store, StoreError};
use kestrel_types::{AgentSpec, RunId, Runtime};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Errors from the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sandbox(#[from] kestrel_sandbox::SandboxError),
    #[error(transparent)]
    Agent(#[from] kestrel_agent::AgentError),
    /// No configured sandbox driver matches the agent's sandbox tier.
    #[error("no sandbox driver available for this agent")]
    NoDriver,
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Builds a fresh runtime adapter. The engine calls it once per run and never shares an adapter
/// across runs. In production this is `kestrel_agent::adapter_for`.
pub type AdapterFactory = Arc<dyn Fn(Runtime) -> Box<dyn RuntimeAdapter> + Send + Sync>;

/// Engine-wide limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Runs past this many stay `Queued` until a slot frees up.
    pub max_concurrent_runs: usize,
    /// How often `run_forever` polls the store for queued runs, control commands and spend.
    pub poll_interval: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 4,
            poll_interval: Duration::from_millis(250),
        }
    }
}

/// What [`Engine::recover`] found and did at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Runs that were `Suspended` and stay that way, resumable as before.
    pub suspended: Vec<RunId>,
    /// Runs that were `Running` when the previous engine stopped. Their sandbox is gone, so they
    /// are now `Suspended(Error("engine restarted"))`.
    pub interrupted: Vec<RunId>,
}

/// The run engine.
pub struct Engine {
    #[allow(dead_code)]
    store: Arc<Store>,
    #[allow(dead_code)]
    drivers: Vec<Box<dyn SandboxDriver>>,
    #[allow(dead_code)]
    adapters: AdapterFactory,
    #[allow(dead_code)]
    gateway_url: Url,
    #[allow(dead_code)]
    limits: Limits,
}

impl Engine {
    /// `drivers` are in preference order: `SandboxTier::Auto` takes the first one.
    pub fn new(
        store: Arc<Store>,
        drivers: Vec<Box<dyn SandboxDriver>>,
        adapters: AdapterFactory,
        gateway_url: Url,
        limits: Limits,
    ) -> Engine {
        Engine {
            store,
            drivers,
            adapters,
            gateway_url,
            limits,
        }
    }

    /// Creates a `Queued` run and writes its `RunCreated` event. The spec is persisted so a later
    /// engine on the same database can resume the run.
    pub async fn submit(&self, spec: AgentSpec, input: String) -> Result<RunId> {
        let _ = (spec, input);
        todo!("submit")
    }

    /// Schedules queued runs, applies control commands and enforces limits until `shutdown`.
    pub async fn run_forever(&self, shutdown: CancellationToken) -> Result<()> {
        let _ = shutdown;
        todo!("run_forever")
    }

    /// Call once at startup, before `run_forever`.
    pub async fn recover(&self) -> Result<RecoveryReport> {
        todo!("recover")
    }
}
