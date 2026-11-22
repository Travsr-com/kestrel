//! Scheduler, run state machine, budgets and approvals.
//!
//! The [`Engine`] drives a run from `Queued` to a terminal status: it provisions a sandbox, launches
//! the agent through its runtime adapter, streams the agent's events into the [`Store`], enforces
//! budgets and the wall clock, and applies control commands (pause, resume, cancel, approve, reject,
//! set budget) read from [`Store::take_control`].
//!
//! Each active run has its own task (a run actor). It is the only writer of that run's status, so
//! control commands are applied while the run is busy, for example while `create` is still blocked.
//!
//! [`caps`] holds the capability grant a run carries and the signed tokens that narrow it per child.

pub mod caps;
mod fetch_home;
mod index;
mod join;
mod slots;
pub mod workspace;
pub use index::{IndexLimits, IndexSource, RunIndexer};
pub use join::{ChildResult, JoinError, JoinMode, JoinOutcome};

/// Why [`Engine::ask_human`] has no answer. The text is what the agent reads.
#[derive(Debug, thiserror::Error)]
pub enum AskError {
    /// Only a running run can ask (an unknown run is not running either).
    #[error("the run is not running, so it cannot ask")]
    NotRunning,
    /// The person declined, with their reason if they gave one.
    #[error("the person declined{}", .0.as_ref().map(|r| format!(": {r}")).unwrap_or_default())]
    Declined(Option<String>),
    /// The run ended while the question waited.
    #[error("the run ended ({0:?}) before the question was answered")]
    Ended(RunStatus),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The Kestrel tools, by the names a capability's `mcp` list uses.
const KESTREL_TOOLS: [&str; 3] = ["kestrel.spawn", "kestrel.join", "kestrel.ask_human"];
pub use workspace::{BroughtBack, WorkspaceError, Workspaces};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use kestrel_agent::{ParsedLine, RunContext, RuntimeAdapter};
use kestrel_sandbox::{
    INSTANCE_LABEL, Ipv4Cidr, LaunchSpec, NetworkPolicy, PreparedNetwork, RUN_LABEL, SandboxDriver,
    SandboxError, SandboxHandle, Subnet, SubnetPool,
};
use kestrel_store::{
    ChildPlan, ControlCommand, RunFilter, RunRow, SpawnRefusal, Store, StoreError,
};
use kestrel_types::{
    AgentSpec, ApprovalId, EventKind, MicroUsd, RunId, RunStatus, Runtime, SandboxTier,
    SuspendReason, Tier, WorkspaceSpec,
};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, Interval, MissedTickBehavior, timeout_at};
use tokio_util::sync::CancellationToken;
use url::Url;

use slots::Slots;

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
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// A `git-clone` repo path that is relative. The engine does not guess a base dir; the
    /// CLI resolves the path when it submits the run.
    #[error("git-clone repo `{}` must be an absolute path", repo.display())]
    RelativeRepo { repo: PathBuf },
    /// The run's config directory (its launch plan's files) could not be written.
    #[error("run config: {0}")]
    RunConfig(String),
    /// `recover` was called while `run_forever` is active.
    #[error("the engine is already running; wait until run_forever has returned")]
    AlreadyRunning,
    /// No block of the subnet pool could be used for the run's network: this run found `taken`
    /// of its blocks taken by networks Kestrel did not know about when it planned the pool, and
    /// no block is free. `held` blocks are held by this engine's own runs and come back to the
    /// pool when those end (a held block under a network made outside Kestrel is not counted);
    /// when there are any, the run can be started again once one of them ends.
    #[error("{}", no_subnet(.pool, *.taken, *.held))]
    NoSubnet {
        pool: Ipv4Cidr,
        taken: usize,
        held: usize,
    },
}

/// The text of [`EngineError::NoSubnet`].
fn no_subnet(pool: &Ipv4Cidr, taken: usize, held: usize) -> String {
    let head = format!(
        "no free network block in subnet pool {pool}: this run found {taken} of the pool's blocks taken by networks made outside Kestrel"
    );
    if held > 0 {
        format!(
            "{head}, and this Kestrel's own runs hold the rest ({held} in use). Start the run again once one of them ends"
        )
    } else {
        format!("{head}, and none is free. Set gateway.subnet_pool to a free range")
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Why [`Engine::spawn_child`] did not spawn. Nothing was created in any of these cases.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The engine has no root key ([`Engine::with_root_key`]), so it cannot check or make
    /// capability tokens.
    #[error("spawning needs capability tokens, and this engine has no root key")]
    NoRootKey,
    #[error("no run {0}")]
    ParentNotFound(RunId),
    /// Only a `Running` run can spawn. A suspended one can again once it is resumed.
    #[error("run {0} is {1:?}; only a running run can spawn")]
    ParentNotRunning(RunId, RunStatus),
    /// The parent's capability does not allow spawning (no `kestrel.spawn`, no spawn rights, no
    /// token, or an expired one).
    #[error("the parent may not spawn: {0}")]
    NotPermitted(String),
    /// The parent's spawn rights reach no deeper (its `max_depth` is used up).
    #[error("the parent may not spawn one more level down (max_depth)")]
    Depth,
    #[error("the parent may not spawn agent `{0}`")]
    AgentNotAllowed(String),
    /// The parent is being cancelled: it may not add to its tree.
    #[error("run {0} is being cancelled")]
    Cancelling(RunId),
    #[error("the parent already has {0} children (max_children)")]
    TooManyChildren(u32),
    #[error("the tree already has {0} runs, the most one tree may have")]
    TooManyRuns(u32),
    /// The carve is not positive, or more than the parent has left.
    #[error("asked for {asked}, but the parent has {remaining} left")]
    Budget {
        asked: MicroUsd,
        remaining: MicroUsd,
    },
    #[error(transparent)]
    Store(StoreError),
}

impl From<StoreError> for SpawnError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Spawn(SpawnRefusal::ParentNotRunning(id, status)) => {
                SpawnError::ParentNotRunning(id, status)
            }
            StoreError::Spawn(SpawnRefusal::Budget { asked, remaining }) => {
                SpawnError::Budget { asked, remaining }
            }
            StoreError::Spawn(SpawnRefusal::TooManyChildren(n)) => SpawnError::TooManyChildren(n),
            StoreError::Spawn(SpawnRefusal::TooManyRuns(n)) => SpawnError::TooManyRuns(n),
            StoreError::NotFound(id) => SpawnError::ParentNotFound(id),
            other => SpawnError::Store(other),
        }
    }
}

/// A child made by [`Engine::spawn_child_with_report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnedChild {
    pub id: RunId,
    /// Set when the child's spec asked for more spawn depth than the parent could give.
    pub lowered_depth: Option<LoweredDepth>,
}

/// A child spec's spawn `max_depth` lowered to what its parent could give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoweredDepth {
    /// What the spec asked for.
    pub asked: u32,
    /// What the child got: the parent's `max_depth` minus one.
    pub granted: u32,
}

impl LoweredDepth {
    /// What was lowered, the part [`LoweredDepth::note`] and [`LoweredDepth::log_line`] share.
    fn lowered(&self) -> String {
        format!("max_depth lowered from {} to {}", self.asked, self.granted)
    }

    /// What the spawning agent is told.
    pub fn note(&self) -> String {
        format!("{} (the most this parent can give)", self.lowered())
    }

    /// The `AgentLog` line recorded on the child.
    pub fn log_line(&self) -> String {
        format!(
            "kestrel: spawn {} (the most its parent can give)",
            self.lowered()
        )
    }
}

/// Default for [`Engine::with_tree_limit`]: the most runs one spawn tree may hold.
pub const DEFAULT_TREE_LIMIT: u32 = 256;

/// How long a capability token is valid. The wall-clock budget is enforced by the engine on its
/// own; this only bounds how long a leaked token stays usable.
const TOKEN_LIFETIME: time::Duration = time::Duration::days(7);

/// Builds a fresh runtime adapter. The engine calls it once per run and never shares an adapter
/// across runs. In production this is `kestrel_agent::adapter_for`.
pub type AdapterFactory = Arc<dyn Fn(Runtime) -> Box<dyn RuntimeAdapter> + Send + Sync>;

/// The checks behind [`Engine::spawn_child`]'s authorisation, in an order that gives the most
/// specific refusal: no `kestrel.spawn` or no spawn rights, then depth, then the agent, then the
/// token's own verdict (run and expiry).
fn authorise(
    held: &caps::Capability,
    token: &caps::CapToken,
    public: &caps::RootPublicKey,
    parent: RunId,
    agent: &str,
    now: time::OffsetDateTime,
) -> std::result::Result<(), SpawnError> {
    if !held.tools.contains(caps::SPAWN_TOOL) {
        return Err(SpawnError::NotPermitted(format!(
            "its capability does not include {}",
            caps::SPAWN_TOOL
        )));
    }
    let Some(limits) = &held.spawn else {
        return Err(SpawnError::NotPermitted(
            "its capability has no spawn rights".into(),
        ));
    };
    if limits.max_depth == 0 {
        return Err(SpawnError::Depth);
    }
    if !limits.agents.contains(agent) {
        return Err(SpawnError::AgentNotAllowed(agent.to_string()));
    }
    let request = caps::Request {
        run: parent,
        action: caps::Action::Spawn {
            agent: agent.to_string(),
        },
        now,
    };
    match caps::verify(token, public, &request) {
        caps::Decision::Allow => Ok(()),
        caps::Decision::Deny(reason) => Err(SpawnError::NotPermitted(reason)),
    }
}

/// The spawn depth `wanted` asked for and what `granted` holds, when `granted` holds less.
fn lowered_depth(wanted: &caps::Capability, granted: &caps::Capability) -> Option<LoweredDepth> {
    let asked = wanted.spawn.as_ref()?.max_depth;
    let got = granted.spawn.as_ref()?.max_depth;
    (got < asked).then_some(LoweredDepth {
        asked,
        granted: got,
    })
}

/// The log line for a `kestrel budget` raise its parent cannot cover.
fn budget_refused(asked: MicroUsd, remaining: MicroUsd) -> String {
    format!(
        "kestrel: budget not changed: the raise needs {asked} from the parent, which has {remaining} left; raise the root's budget first"
    )
}

/// `spec.capabilities` for a grant.
fn granted_capabilities(
    cap: &caps::Capability,
    keep_children: bool,
) -> kestrel_types::Capabilities {
    kestrel_types::Capabilities {
        mcp: cap.tools.iter().cloned().collect(),
        network: cap.hosts.iter().cloned().collect(),
        secrets: cap.secrets.iter().cloned().collect(),
        spawn: cap.spawn.as_ref().map(|s| kestrel_types::SpawnPolicy {
            max_children: s.max_children,
            max_depth: s.max_depth,
            agents: s.agents.iter().cloned().collect(),
            keep_children,
        }),
    }
}

/// The spec a child runs with: one tier below the parent unless it states a `model_policy`, and
/// the parent's repo when its `git-clone` repo is relative.
fn child_spec(child: &AgentSpec, parent: &AgentSpec) -> AgentSpec {
    let mut spec = child.clone();
    if !spec.model_policy.stated {
        spec.model_policy.default = parent.model_policy.default.below();
    }
    if let (WorkspaceSpec::GitClone { repo, .. }, WorkspaceSpec::GitClone { repo: theirs, .. }) =
        (&mut spec.sandbox.workspace, &parent.sandbox.workspace)
        && repo.is_relative()
    {
        repo.clone_from(theirs);
    }
    spec
}

/// Engine-wide limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Concurrency slots. Runs past this many stay `Queued` until a slot frees up. See the
    /// [`Engine`] docs for which runs hold a slot.
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

/// Provider model name for each model tier. A tier missing from the map uses its default.
pub type ModelMap = HashMap<Tier, String>;

/// The model a tier maps to when the [`ModelMap`] does not name one. These are defaults only:
/// override them in config to follow new model releases or another provider.
pub fn default_model(tier: Tier) -> &'static str {
    match tier {
        Tier::Cheap => "claude-haiku-5-5",
        Tier::Mid => "claude-sonnet-5-5",
        Tier::Frontier => "claude-opus-5-5",
    }
}

/// The default [`ModelMap`]: every tier mapped to its [`default_model`].
pub fn default_models() -> ModelMap {
    [Tier::Cheap, Tier::Mid, Tier::Frontier]
        .into_iter()
        .map(|t| (t, default_model(t).to_string()))
        .collect()
}

/// What [`Engine::recover`] found and did at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Runs that were `Suspended` and stay that way, resumable as before.
    pub suspended: Vec<RunId>,
    /// Runs that were `Running` when the previous engine stopped. Their sandbox is gone, so they
    /// are now `Suspended(Error("engine restarted"))`.
    pub interrupted: Vec<RunId>,
    /// Why the Travsr copies kept from before could not be listed, so none was forgotten (see
    /// [`Engine::with_indexer`]). `None` when they were, or there is no indexer.
    pub travsr_unlisted: Option<String>,
}

/// What [`Engine::shutdown`] could and could not clean up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Runs whose sandbox was destroyed.
    pub destroyed: Vec<RunId>,
    /// Runs whose sandbox could not be destroyed, with the driver's error text.
    pub failed: Vec<(RunId, String)>,
    /// Runs whose sandbox was still being destroyed (or created) when the grace period ended.
    pub timed_out: Vec<RunId>,
}

/// Status message of a run that was `Running` when the previous engine stopped.
const ENGINE_RESTARTED: &str = "engine restarted";
/// Failure message of a run past its wall clock limit.
const WALL_CLOCK_EXCEEDED: &str = "wall clock exceeded";

/// The run engine.
///
/// # Concurrency slots
///
/// A run holds one of `Limits::max_concurrent_runs` slots only while its sandbox runs or is being
/// provisioned: `Provisioning`, `Running` and `Suspended(Approval)` (the sandbox keeps running
/// while an approval is pending). `Suspended(Budget | Manual | Error)` has a stopped sandbox and
/// holds no slot, so a parent waiting on children (M2) cannot deadlock the pool. A terminal run
/// releases its slot once, after its sandbox is destroyed.
///
/// Resuming a run whose sandbox is stopped needs a free slot (a `Resume`, or an `Approve` after a
/// restart). If none is free the command stays pending: the run stays suspended and the command
/// is not dropped (the one exception is an exhausted subnet pool, see "Run networks" below).
/// `SetBudget` and `Cancel` still apply at once, and a `Cancel` removes the pending resume. A
/// resume still pending when the engine stops is lost and must be sent again
/// (the run stays suspended). After [`Engine::recover`] no run holds a slot, and commands for the
/// recovered runs wait for one like any other.
///
/// Everything waiting for a slot, `Queued` runs and pending resumes alike, is served in one FIFO
/// order. Each waiter gets an arrival number. Within one scheduler tick, newly seen `Queued` runs
/// are numbered first (in creation order), then newly claimed resume commands (in control id
/// order). Slots go to the lowest arrival number first.
///
/// # Run networks
///
/// With [`Engine::with_subnets`] every run that is about to get a NEW sandbox (a `Queued` run, or
/// a resume of a run that has no sandbox, as after a restart) also needs a `/29` block of the
/// subnet pool. The scheduler reserves it together with the slot, in the same FIFO line. A waiter
/// that gets a slot but no block gives the slot back and stays where it is (the run stays
/// `Queued`, the resume stays pending). It gets one `AgentLog` naming the subnet pool for the
/// whole wait, not one per tick. Waiters behind it that also need a block wait too; ones that
/// need none (a resume of a run whose stopped sandbox still exists) may take a free slot, so
/// runs that hold blocks while suspended can always be resumed.
///
/// When the pool is exhausted ([`SubnetPool::exhausted`]: nothing is free and no block in use will
/// come back until the next start), waiting would never end. Then each waiter that needs a block
/// stops waiting as soon as it reaches a free slot (at once when slots are free, else when one
/// frees up): a queued run fails with a message naming the pool, and a resume or approve is
/// dropped with a log line naming it while the run stays suspended.
///
/// The run actor turns the block into the run's network with `SandboxDriver::prepare_network`
/// before it builds the launch plan, and the gateway URL of the run points at the bridge on that
/// network. A block docker says is taken by another network is excluded from the pool, together
/// with every block the driver's `taken_subnets` lists in use, and another one is tried, until
/// the pool has no free block left; only then does the run fail. Its failure message says how
/// many blocks the run found taken and whether this engine's own runs hold the rest. A run
/// that already has a network (its stopped sandbox outlived a `run_forever` that was stopped and
/// started again) reuses it. A driver without run networks gives the block back at once, and the
/// run uses the shared network as before. When the run ends, its sandbox is destroyed, then its
/// network removed, then its block freed. A suspended run keeps its network
/// and block while its stopped sandbox exists. [`Engine::shutdown`] removes the networks of the
/// sandboxes it destroys, and [`Engine::recover`] removes every leftover labelled run network.
///
/// # Stopping and starting again
///
/// [`Engine::shutdown`] stops the scheduler, then destroys every sandbox this engine created and
/// has not destroyed yet. That includes a sandbox whose `create` is still running and one in the
/// final destroy of a finished run. It changes no run status: a `Running` run stays `Running` in
/// the store, and [`Engine::recover`] suspends it on the next start.
///
/// If the process dies instead, [`Engine::recover`] destroys the sandboxes recorded in the
/// events, and everything the drivers list as left over: a container with the label
/// `kestrel.run=<run id>` that no event mentions, for example one whose `create` was cut short.
/// With [`Engine::with_instance`] every sandbox also carries `kestrel.instance=<id>`, and drivers
/// built with the same id list only those, so two Kestrel instances on one Docker host leave each
/// other's sandboxes alone. Without an instance id the sweep covers every `kestrel.run` container
/// on the host. Apple `container` cannot list by label, so nothing is swept there.
///
/// `run_forever` gives every slot back and clears the waiting line when it starts and when it
/// ends, so the same engine can run again. `recover` and a second `run_forever` are refused while
/// one is active.
///
/// The gateway token of a run is replaced by `[token]` in everything the engine records from the
/// agent's output, and in the failure text of a run.
///
/// Known M1 limitations:
///
/// * Resuming a paused or budget-suspended run starts the stopped sandbox again, which restarts
///   the agent command from the beginning. The container survives, the agent's process does not.
/// * A run resumed by a new engine after a restart gets a fresh sandbox, and its wall clock starts
///   over.
/// * The wall clock counts only time spent `Provisioning` or `Running`, not time suspended.
/// * A run that hits taken blocks while the rest of the pool is held by its own runs fails with
///   a message saying so, instead of waiting for a block.
pub struct Engine {
    inner: Arc<Inner>,
}

/// Sets the permission bits of `path` to `mode`, whatever the umask. Nothing to set off unix.
#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Whether `dir` (with metadata `meta`) belongs to this process's user. A file made in it is
/// owned by that user, which is how the user id is learnt without `unsafe`; the file is removed
/// again. A directory this user cannot write to is an error. Always true off unix.
#[cfg(unix)]
fn owned_by_us(dir: &std::path::Path, meta: &std::fs::Metadata) -> std::io::Result<bool> {
    use std::hash::BuildHasher;
    use std::os::unix::fs::MetadataExt;
    let nonce = std::collections::hash_map::RandomState::new().hash_one(std::process::id());
    let probe = dir.join(format!(".owner-{nonce:016x}"));
    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    let ours = made.metadata().map(|m| m.uid());
    let _ = std::fs::remove_file(&probe);
    Ok(ours? == meta.uid())
}

#[cfg(not(unix))]
fn owned_by_us(_dir: &std::path::Path, _meta: &std::fs::Metadata) -> std::io::Result<bool> {
    Ok(true)
}

/// A run config root of this engine only: a temp directory with a random name, so no other user
/// can make it first.
fn default_run_config_root() -> PathBuf {
    use std::hash::BuildHasher;
    let nonce = std::collections::hash_map::RandomState::new().hash_one(std::process::id());
    std::env::temp_dir().join(format!("kestrel-run-config-{nonce:016x}"))
}

struct Inner {
    store: Arc<Store>,
    drivers: Vec<Box<dyn SandboxDriver>>,
    adapters: AdapterFactory,
    gateway_url: Url,
    limits: Limits,
    models: ModelMap,
    /// Per-run workspaces, when configured with [`Engine::with_workspaces`].
    workspaces: Option<Arc<Workspaces>>,
    /// Who holds a concurrency slot. Written by the scheduler (acquire) and the run actors
    /// (release), so it sits behind a lock.
    slots: Mutex<Slots>,
    /// Instance id for the `kestrel.instance` label, see [`Engine::with_instance`].
    instance: Option<String>,
    /// Blocks for run networks, see [`Engine::with_subnets`].
    subnets: Option<Arc<SubnetPool>>,
    /// Signs and checks capability tokens, see [`Engine::with_root_key`].
    root_key: Option<caps::RootKey>,
    /// Where each run's config directory goes, see [`Engine::with_run_config`].
    run_config: PathBuf,
    /// Most runs in one spawn tree, see [`Engine::with_tree_limit`].
    tree_limit: u32,
    /// What lives outside the run actors, which are aborted when `run_forever` returns.
    live: Mutex<Live>,
    /// Parents leaving a join that wait for their slot back, picked up by the scheduler.
    rejoins: Mutex<Vec<(RunId, Rejoin)>>,
    /// Joins waiting, by parent, and whether the first one gave the parent's slot back.
    joins: Mutex<HashMap<RunId, JoinState>>,
    /// Runs a cancel was asked for, until it is delivered. `spawn_child` refuses them.
    cancelling: Mutex<HashSet<RunId>>,
    /// Indexes runs' clones for Travsr, see [`Engine::with_indexer`].
    indexing: Option<Arc<index::Indexing>>,
    /// Signals a change of `live`.
    changed: Notify,
}

/// Sandboxes and scheduler state that must outlive the run actors.
#[derive(Default)]
struct Live {
    /// Stops `run_forever` while it is active.
    active: Option<CancellationToken>,
    /// `create` calls in flight, by run.
    creating: HashSet<RunId>,
    /// Sandboxes created and not yet destroyed: (driver index, sandbox id) to run.
    sandboxes: BTreeMap<(usize, String), RunId>,
    /// Blocks of the subnet pool held by a run: reserved by the scheduler, then used by the run's
    /// network.
    blocks: HashMap<RunId, Subnet>,
    /// Run networks made and not yet removed, with the index of the driver that made them.
    networks: HashMap<RunId, (usize, PreparedNetwork)>,
}

/// Control channels of the run actors, by run.
type Actors = HashMap<RunId, mpsc::UnboundedSender<ControlCommand>>;

/// What `run_forever` keeps between ticks.
#[derive(Default)]
struct Scheduler {
    actors: Actors,
    /// Dropping the set aborts every run actor.
    tasks: JoinSet<()>,
    /// Everything waiting for a slot, in arrival order (the front is served first).
    waiting: VecDeque<Waiter>,
    /// Cancels held back. Each is delivered once nothing live is below its run, so a tree is
    /// cancelled from the leaves up whatever the order here.
    pending_cancels: Vec<RunId>,
    /// Set by `run_forever`: when the scheduler goes (the loop ended, or its future was
    /// dropped), the held-back cancels go back into the store for the next start.
    requeue: Option<Arc<Inner>>,
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        if let Some(inner) = &self.requeue {
            inner.requeue_cancels(&self.pending_cancels);
        }
    }
}

/// A queued run, a resume command that needs a slot, or a parent leaving a join.
struct Waiter {
    run: RunId,
    /// `None` for a `Queued` run to start.
    resume: Option<ControlCommand>,
    /// Whether the run was told it waits for a network block, so it is told once per wait.
    told: bool,
    /// A parent that gave its slot back for a join and wants it again.
    rejoin: Option<Rejoin>,
}

/// How a parent leaving a join waits for its slot.
enum Rejoin {
    /// The join waits: told once the slot is back. Dropped unanswered if the join went away.
    Told(oneshot::Sender<()>),
    /// Nobody waits (the join was dropped or timed out): the slot is simply taken back.
    Detached,
}

/// The joins of one parent that are waiting.
#[derive(Default)]
struct JoinState {
    waiting: usize,
    /// Whether the first join gave the parent's slot back, so the last one takes it again.
    released: bool,
    /// The last join left and the slot is on its way back. A join that starts now takes that
    /// over (it will take the slot back when it ends), and the scheduler grants no slot to a
    /// parent while one of its joins waits.
    regaining: bool,
}

impl Waiter {
    fn is_resume_of(&self, run: RunId) -> bool {
        self.run == run && self.resume.is_some()
    }
}

impl Engine {
    /// `drivers` are in preference order: `SandboxTier::Auto` takes the first one. Model tiers
    /// map to [`default_models`]; use [`Engine::with_models`] to choose them.
    pub fn new(
        store: Arc<Store>,
        drivers: Vec<Box<dyn SandboxDriver>>,
        adapters: AdapterFactory,
        gateway_url: Url,
        limits: Limits,
    ) -> Engine {
        Self::with_models(
            store,
            drivers,
            adapters,
            gateway_url,
            limits,
            default_models(),
        )
    }

    /// Like [`Engine::new`], with the model name for each tier taken from `models`.
    pub fn with_models(
        store: Arc<Store>,
        drivers: Vec<Box<dyn SandboxDriver>>,
        adapters: AdapterFactory,
        gateway_url: Url,
        limits: Limits,
        models: ModelMap,
    ) -> Engine {
        Engine {
            inner: Arc::new(Inner {
                store,
                drivers,
                adapters,
                gateway_url,
                slots: Mutex::new(Slots::new(limits.max_concurrent_runs)),
                limits,
                models,
                workspaces: None,
                instance: None,
                subnets: None,
                root_key: None,
                run_config: default_run_config_root(),
                tree_limit: DEFAULT_TREE_LIMIT,
                live: Mutex::default(),
                rejoins: Mutex::default(),
                joins: Mutex::default(),
                cancelling: Mutex::default(),
                indexing: None,
                changed: Notify::new(),
            }),
        }
    }

    /// Gives every run a workspace from `workspaces`, mounted at `/workspace` in its sandbox
    /// (`LaunchSpec.workspace`). The workspace is prepared before the sandbox is created; a run
    /// whose workspace cannot be prepared fails without a sandbox. A `git-clone` repo path
    /// must be absolute (the CLI resolves it at submit time); a relative one fails the run.
    ///
    /// Without this, runs get no workspace (`LaunchSpec.workspace` is `None`), as before.
    /// This cannot silently do nothing: it takes the engine by value, and the only way to share
    /// its state (`start`, `recover`, `run_forever`) borrows it and keeps no clone alive in the
    /// returned value, so the state is never shared when this runs.
    pub fn with_workspaces(mut self, workspaces: Workspaces) -> Engine {
        let inner = Arc::get_mut(&mut self.inner);
        debug_assert!(inner.is_some(), "engine state is shared");
        if let Some(inner) = inner {
            inner.workspaces = Some(Arc::new(workspaces));
        }
        self
    }

    /// Labels every sandbox with `kestrel.instance=<id>`, see the [`Engine`] docs. Use the same
    /// id for the drivers' `with_instance`. Takes the engine by value, like
    /// [`Engine::with_workspaces`].
    pub fn with_instance(mut self, id: impl Into<String>) -> Engine {
        let inner = Arc::get_mut(&mut self.inner);
        debug_assert!(inner.is_some(), "engine state is shared");
        if let Some(inner) = inner {
            inner.instance = Some(id.into());
        }
        self
    }

    /// Gives every run its own network on a `/29` block of `pool`, see the [`Engine`] docs.
    /// Drivers that return `None` from `prepare_network` keep the shared network. Takes the
    /// engine by value, like [`Engine::with_workspaces`].
    pub fn with_subnets(mut self, pool: Arc<SubnetPool>) -> Engine {
        let inner = Arc::get_mut(&mut self.inner);
        debug_assert!(inner.is_some(), "engine state is shared");
        if let Some(inner) = inner {
            inner.subnets = Some(pool);
        }
        self
    }

    /// Each run's config directory (the files of its launch plan, such as the MCP config) goes in
    /// `root`, as `{root}/{run id}`, mounted read-only at `/kestrel` in the sandbox and removed
    /// when the run's sandbox goes. `root` is made its owner's only, so no other host user reaches
    /// a run's token; a run's directory is readable by any user, so the sandbox can read it
    /// whichever user its image runs as. A `root` that is a link, not a directory, or owned by
    /// another user fails the run. Put it where no other user can rename or replace it: its
    /// parent is not checked.
    ///
    /// Without this, a temp directory with a random name, of this engine only. It is left behind
    /// when the engine goes, and a later engine's [`Engine::recover`] cannot find what a killed
    /// one left in it, so set a fixed root (as `kestrel serve` does) for anything long-lived.
    pub fn with_run_config(mut self, root: PathBuf) -> Engine {
        let inner = Arc::get_mut(&mut self.inner);
        debug_assert!(inner.is_some(), "engine state is shared");
        if let Some(inner) = inner {
            inner.run_config = root;
        }
        self
    }

    /// Signs capability tokens with `key`, which [`Engine::spawn_child`] needs. Use the same key
    /// across restarts: tokens in the store are checked against it. Takes the engine by value,
    /// like [`Engine::with_workspaces`].
    pub fn with_root_key(mut self, key: caps::RootKey) -> Engine {
        let inner = Arc::get_mut(&mut self.inner);
        debug_assert!(inner.is_some(), "engine state is shared");
        if let Some(inner) = inner {
            inner.root_key = Some(key);
        }
        self
    }

    /// The most runs one spawn tree may hold, its root included ([`DEFAULT_TREE_LIMIT`] unless
    /// set). A spawn past it fails with [`SpawnError::TooManyRuns`]. Takes the engine by value.
    pub fn with_tree_limit(mut self, runs: u32) -> Engine {
        let inner = Arc::get_mut(&mut self.inner);
        debug_assert!(inner.is_some(), "engine state is shared");
        if let Some(inner) = inner {
            inner.tree_limit = runs;
        }
        self
    }

    /// Has each run's code indexed for Travsr by `indexer`, so its agent can use Travsr's tools.
    /// Takes the engine by value, like [`Engine::with_workspaces`], which it needs.
    ///
    /// * **Which runs**: those with a `git-clone` workspace whose capability grants a
    ///   `travsr.<tool>`. A child is indexed on its own.
    /// * **What**: never the run's workspace, which the agent may rewrite and Travsr would read
    ///   on the host. The indexer gets an [`IndexSource`]: the source repo, and the commit the
    ///   workspace was cloned at ([`Workspaces::head`], read before the sandbox exists). It keeps
    ///   its own copy of that commit. A resumed run reuses the workspace it had, so its source
    ///   has no commit: the indexer uses the one it recorded. The index therefore shows the code
    ///   the run started from, never the agent's edits.
    /// * **When**: after the workspace exists and before the agent starts (while the sandbox is
    ///   made). At most `limits.max_jobs` jobs run at once. A run waits at most `limits.timeout`
    ///   for its index, the wait for a free job slot included. A job that fails or takes longer
    ///   does not hold the run: it starts without Travsr, and its log says why.
    ///   [`Engine::travsr_tools`] then lists nothing for it.
    /// * **Released** (served no more, its copy kept for a resume) when [`Engine::shutdown`]
    ///   stops a run.
    /// * **Forgotten** (nothing kept) when the run ends, after its slot is given back, and by
    ///   [`Engine::recover`] for every copy the indexer keeps ([`RunIndexer::kept`]) whose run
    ///   ended or no longer exists. Only a run handed to the indexer (it got a job slot), or
    ///   listed as kept, is forgotten; any other run never reaches the indexer. Each forget may
    ///   take `limits.forget_timeout`. One that fails is logged on the run and tried again every
    ///   `limits.retry_every` while `run_forever` runs. It never fails the run.
    pub fn with_indexer(mut self, indexer: Arc<dyn RunIndexer>, limits: IndexLimits) -> Engine {
        let inner = Arc::get_mut(&mut self.inner);
        debug_assert!(inner.is_some(), "engine state is shared");
        if let Some(inner) = inner {
            inner.indexing = Some(Arc::new(index::Indexing::new(indexer, limits)));
        }
        self
    }

    /// The Travsr tools `run` may call, by Travsr's names (`get_callers` for
    /// `travsr.get_callers`), in its capability's order; `None` once it has ended (or for an
    /// unknown run). Empty unless its clone is indexed (see [`Engine::with_indexer`]).
    pub fn travsr_tools(&self, run: RunId) -> Result<Option<Vec<String>>> {
        let store = &self.inner.store;
        let Some(row) = store.get_run(run)? else {
            return Ok(None);
        };
        if row.status.is_terminal() {
            return Ok(None);
        }
        let spec = if self.travsr_ready(run) {
            store.run_spec(run)?
        } else {
            None
        };
        let Some(spec) = spec else {
            return Ok(Some(Vec::new()));
        };
        // A spawned run's stored spec holds only what it was granted, as for `kestrel_tools`.
        let mut tools: Vec<String> = Vec::new();
        for tool in spec
            .capabilities
            .mcp
            .iter()
            .filter_map(|name| name.strip_prefix("travsr."))
        {
            if !tools.iter().any(|t| t == tool) {
                tools.push(tool.to_string());
            }
        }
        Ok(Some(tools))
    }

    /// Whether `run`'s code is indexed for Travsr and served, so the gateway may ask the indexer
    /// about it (the indexer knows the run's copy by its id).
    pub fn travsr_ready(&self, run: RunId) -> bool {
        self.inner.indexing.as_ref().is_some_and(|ix| ix.ready(run))
    }

    /// The Kestrel tools `run` may call, in the order spawn, join, ask_human; `None` once it has
    /// ended (or for an unknown run). A tool must be in the run's capability, and
    /// `kestrel.spawn` also needs spawn rights. A spawned run's stored spec holds only what it was
    /// granted, so its capability is read from that spec; spawning itself checks the token.
    pub fn kestrel_tools(&self, run: RunId) -> Result<Option<Vec<&'static str>>> {
        let store = &self.inner.store;
        let Some(row) = store.get_run(run)? else {
            return Ok(None);
        };
        if row.status.is_terminal() {
            return Ok(None);
        }
        let Some(spec) = store.run_spec(run)? else {
            return Ok(Some(Vec::new()));
        };
        let now = time::OffsetDateTime::now_utc();
        let held = caps::Capability::from_spec(run, &spec, now + TOKEN_LIFETIME);
        Ok(Some(
            KESTREL_TOOLS
                .into_iter()
                .filter(|tool| held.allows(&caps::Action::Tool((*tool).into())).is_ok())
                .filter(|tool| *tool != "kestrel.spawn" || held.spawn.is_some())
                .collect(),
        ))
    }

    /// Whether approval `id` of `run` has been decided.
    fn is_resolved(&self, run: RunId, id: ApprovalId) -> Result<bool> {
        Ok(self.inner.store.events_since(run, 0)?.into_iter().any(
            |e| matches!(e.kind, EventKind::ApprovalResolved { id: decided, .. } if decided == id),
        ))
    }

    /// Whether approval `id` of `run` is a question (`kestrel.ask_human`).
    fn is_question(&self, run: RunId, id: ApprovalId) -> Result<bool> {
        Ok(self.inner.store.events_since(run, 0)?.into_iter().any(|e| {
            matches!(e.kind, EventKind::ApprovalRequested { id: asked, question: true, .. } if asked == id)
        }))
    }

    /// Records that `run` called `tool`, which its capabilities do not allow.
    pub fn tool_denied(&self, run: RunId, tool: &str) -> Result<()> {
        self.inner.store.append(
            run,
            EventKind::AgentLog {
                line: format!(
                    "kestrel: {tool} is not permitted for this run, the call was refused"
                ),
            },
        )?;
        Ok(())
    }

    /// Records on `run` that its call of `tool` was refused for lack of room (the gateway holds
    /// too many calls open), with the reason its agent was given.
    ///
    /// # Errors
    /// The store's, when the event cannot be written.
    pub fn tool_refused(&self, run: RunId, tool: &str, reason: &str) -> Result<()> {
        self.inner.store.append(
            run,
            EventKind::AgentLog {
                line: format!("kestrel: {tool} was refused: {reason}"),
            },
        )?;
        Ok(())
    }

    /// Asks a person `question` for `run` (`kestrel.ask_human`) and waits for the answer. The run
    /// records it as a question approval and waits on it (its sandbox keeps running, in the tool
    /// call); approving with text answers it. Declining fails the run, as any rejection does.
    pub async fn ask_human(
        &self,
        run: RunId,
        question: String,
    ) -> std::result::Result<String, AskError> {
        let store = &self.inner.store;
        let running = store
            .get_run(run)?
            .is_some_and(|r| r.status == RunStatus::Running);
        if !running {
            return Err(AskError::NotRunning);
        }
        let id = ApprovalId::new();
        // The run's own actor records it, so the request and the wait happen in its order.
        store.push_control(run, ControlCommand::Ask { id, question })?;
        let poll = self
            .inner
            .limits
            .poll_interval
            .max(Duration::from_millis(1));
        let mut cursor = 0;
        // Whether the run recorded the question: one that stopped running first refuses it unasked.
        let mut asked = false;
        loop {
            // Every new event, a page at a time until a short one, so a long log is read once
            // (not a page per poll) and never held whole.
            loop {
                let page = store.events_page(run, cursor, 256)?;
                let full = page.len() == 256;
                for event in page {
                    cursor = event.seq;
                    match event.kind {
                        EventKind::ApprovalRequested { id: requested, .. } if requested == id => {
                            asked = true;
                        }
                        EventKind::ApprovalResolved {
                            id: resolved,
                            approved,
                            payload,
                        } if resolved == id => {
                            return match (asked, approved) {
                                (false, _) => Err(AskError::NotRunning),
                                (true, true) => Ok(payload.unwrap_or_default()),
                                (true, false) => Err(AskError::Declined(payload)),
                            };
                        }
                        _ => {}
                    }
                }
                if !full {
                    break;
                }
            }
            if let Some(row) = store.get_run(run)?
                && row.status.is_terminal()
            {
                return Err(AskError::Ended(row.status));
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Creates a child run of `parent` from `child` and returns its id. The child starts like
    /// any run: it waits `Queued` for a slot, and gets its own workspace.
    ///
    /// * **Authorised by the parent's capability token**: it needs `kestrel.spawn`, spawn rights
    ///   with depth left, and `child.name` among its spawnable agents. A root run's token is made
    ///   from its spec on its first spawn.
    /// * **The child's token** is the parent's, attenuated to what the child's spec asks for and
    ///   the parent holds. What the spec asked for beyond that is recorded on the child as
    ///   `CapabilitiesDropped`.
    /// * **Budget**: `budget` is carved out of what the parent has left, atomically (see
    ///   [`Store::spawn_child`]). Without one, the child gets the smaller of its spec's budget and
    ///   half of what the parent has left. A finished child returns what it did not spend.
    /// * **Limits**: the parent's `max_children` (live and finished) and the tree limit
    ///   ([`Engine::with_tree_limit`]).
    /// * **Model tier**: a child spec without `model_policy` runs one tier below its parent.
    /// * **Workspace**: a `git-clone` child with a relative repo clones its parent's repo.
    /// * **Depth**: a spec asking for more spawn `max_depth` than the parent can give (its own
    ///   minus one) gets what the parent can give. The child records it in an `AgentLog`, and
    ///   [`Engine::spawn_child_with_report`] returns it, so the caller can tell the agent.
    pub async fn spawn_child(
        &self,
        parent: RunId,
        child: &AgentSpec,
        input: &str,
        budget: Option<MicroUsd>,
    ) -> std::result::Result<RunId, SpawnError> {
        self.spawn_child_with_report(parent, child, input, budget)
            .await
            .map(|spawned| spawned.id)
    }

    /// [`Engine::spawn_child`], also saying whether the child's spawn depth was lowered.
    pub async fn spawn_child_with_report(
        &self,
        parent: RunId,
        child: &AgentSpec,
        input: &str,
        budget: Option<MicroUsd>,
    ) -> std::result::Result<SpawnedChild, SpawnError> {
        let inner = &self.inner;
        let key = inner.root_key.as_ref().ok_or(SpawnError::NoRootKey)?;
        let store = &inner.store;
        let row = store
            .get_run(parent)?
            .ok_or(SpawnError::ParentNotFound(parent))?;
        if row.status != RunStatus::Running {
            return Err(SpawnError::ParentNotRunning(parent, row.status));
        }
        let parent_spec = store
            .run_spec(parent)?
            .ok_or(SpawnError::ParentNotFound(parent))?;
        if inner.is_cancelling(parent) {
            return Err(SpawnError::Cancelling(parent));
        }
        let now = time::OffsetDateTime::now_utc();
        let token = self.token_of(&row, &parent_spec, key, now)?;
        let public = key.public();
        let held =
            caps::grant(&token, &public).map_err(|e| SpawnError::NotPermitted(e.to_string()))?;
        authorise(&held, &token, &public, parent, &child.name, now)?;
        let max_children = held.spawn.as_ref().map_or(0, |s| s.max_children);

        let mut spec = child_spec(child, &parent_spec);
        let wanted = caps::Capability::from_spec(parent, &spec, now + TOKEN_LIFETIME);
        let (narrowed, dropped) = caps::narrow(&held, &wanted);
        // The stored spec says only what the child was granted, so nothing that reads it later
        // can see more than its token holds.
        // A child keeps its children only if its parent keeps its own.
        let keeps = |s: &AgentSpec| {
            s.capabilities
                .spawn
                .as_ref()
                .is_some_and(|p| p.keep_children)
        };
        let keep = keeps(&spec) && keeps(&parent_spec);
        spec.capabilities = granted_capabilities(&narrowed, keep);
        let mut child_events = Vec::new();
        let lowered_depth = lowered_depth(&wanted, &narrowed);
        if let Some(lowered) = lowered_depth {
            child_events.push(EventKind::AgentLog {
                line: lowered.log_line(),
            });
        }
        if !dropped.is_empty() {
            child_events.push(EventKind::CapabilitiesDropped {
                tools: dropped.tools,
                hosts: dropped.hosts,
                secrets: dropped.secrets,
                agents: dropped.agents,
            });
        }
        let plan = ChildPlan {
            spec: &spec,
            input,
            budget,
            max_children,
            max_tree_runs: inner.tree_limit,
            child_events,
        };
        let spawned = store.spawn_child(parent, plan, |id, carve| {
            let mut cap = narrowed.clone();
            cap.run = id;
            // The token's budget is a ceiling: never more than the parent's grant.
            cap.budget = carve.min(held.budget);
            caps::attenuate(&token, key, &cap)
                .map(|t| Some(t.as_bytes().to_vec()))
                .map_err(|e| StoreError::Capability(e.to_string()))
        })?;
        Ok(SpawnedChild {
            id: spawned.id,
            lowered_depth,
        })
    }

    /// The parent's capability token. A root run without one gets one from its spec; any other
    /// run without one may not spawn.
    fn token_of(
        &self,
        row: &RunRow,
        spec: &AgentSpec,
        key: &caps::RootKey,
        now: time::OffsetDateTime,
    ) -> std::result::Result<caps::CapToken, SpawnError> {
        let store = &self.inner.store;
        if let Some(bytes) = store.run_cap(row.id)? {
            let token = caps::CapToken::from_bytes(bytes);
            // A spawned run's token comes from its parent's and cannot be remade here. A root's
            // comes from its spec, so an expired (or unreadable) one is made again below.
            let fresh = caps::grant(&token, &key.public()).is_ok_and(|c| c.expires_at > now);
            if row.parent.is_some() || fresh {
                return Ok(token);
            }
        } else if row.parent.is_some() {
            return Err(SpawnError::NotPermitted(
                "the run has no capability token".into(),
            ));
        }
        let cap = caps::Capability::from_spec(row.id, spec, now + TOKEN_LIFETIME);
        let token = caps::issue(key, &cap).map_err(|e| SpawnError::NotPermitted(e.to_string()))?;
        store.set_run_cap(row.id, token.as_bytes())?;
        Ok(token)
    }

    /// How many concurrency slots are held right now. For diagnostics and tests.
    pub fn slots_in_use(&self) -> usize {
        self.inner.slots().used()
    }

    /// Whether `run` holds a concurrency slot right now (a parent in a join holds none). For
    /// diagnostics and tests.
    pub fn holds_slot(&self, run: RunId) -> bool {
        self.inner.slots().holds(run)
    }

    /// Creates a `Queued` run and writes its `RunCreated` event. The spec is persisted so a later
    /// engine on the same database can resume the run.
    pub async fn submit(&self, spec: AgentSpec, input: String) -> Result<RunId> {
        let store = &self.inner.store;
        let id = store.create_run(&spec, &input)?;
        store.append(
            id,
            EventKind::RunCreated {
                agent: spec.name,
                parent: None,
            },
        )?;
        Ok(id)
    }

    /// Schedules queued runs, applies control commands and enforces limits until `shutdown`.
    ///
    /// When `shutdown` is cancelled (or [`Engine::shutdown`] runs) the run actors are dropped
    /// without touching their runs; [`Engine::recover`] picks them up on the next start. Their
    /// sandboxes are left for [`Engine::shutdown`] or the next `recover` to destroy.
    ///
    /// A store error that breaks a whole tick ends this with [`EngineError::Store`]. A store
    /// error on one control command is written to that run's log and scheduling goes on.
    pub async fn run_forever(&self, shutdown: CancellationToken) -> Result<()> {
        let stop = CancellationToken::new();
        let _active = self.inner.activate(&stop)?;
        // Dropping the scheduler on return aborts every run actor, before `_active` resets slots.
        let mut scheduler = Scheduler::default();
        scheduler.requeue = Some(self.inner.clone());
        let mut tick = self.inner.ticker();
        // Failed Travsr forgets are tried again now and then, in a task of their own.
        let indexing = self.inner.indexing.clone();
        let mut retry = tokio::time::interval(
            indexing
                .as_ref()
                .map_or(Duration::from_secs(3600), |ix| ix.retry_every())
                .max(Duration::from_millis(1)),
        );
        retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let result = loop {
            tokio::select! {
                _ = shutdown.cancelled() => break Ok(()),
                _ = stop.cancelled() => break Ok(()),
                _ = tick.tick() => {
                    if let Err(e) = self.inner.poll(&mut scheduler) {
                        break Err(e);
                    }
                }
                _ = retry.tick(), if indexing.is_some() => {
                    if let Some(ix) = indexing.clone() {
                        scheduler.tasks.spawn(async move { ix.retry().await });
                    }
                }
            }
        };
        // Dropping the scheduler puts a cancel still held back into the store again.
        result
    }

    /// Stops `run_forever` and destroys every sandbox this engine created and has not destroyed,
    /// waiting at most `grace` in total. `run_forever` has returned by the time this does,
    /// unless the grace period ran out first. No run status changes.
    ///
    /// The report lists the runs whose sandbox was destroyed, the ones where the driver failed
    /// (with its error text) and the ones still busy when `grace` ended. A second call, or a call
    /// with nothing running, returns an empty report.
    pub async fn shutdown(&self, grace: Duration) -> ShutdownReport {
        let inner = &self.inner;
        let deadline = Instant::now() + grace;
        let stop = inner.live().active.clone();
        if let Some(stop) = stop {
            stop.cancel();
        }
        // The actors are gone once `run_forever` returned; a `create` may still be running.
        let mut report = ShutdownReport::default();
        if timeout_at(deadline, inner.settled()).await.is_err() {
            report
                .timed_out
                .extend(inner.live().creating.iter().copied());
        }
        let found: Vec<_> = inner.live().sandboxes.clone().into_iter().collect();
        let mut left: Vec<RunId> = found.iter().map(|(_, run)| *run).collect();
        let mut pending: FuturesUnordered<_> = found
            .into_iter()
            .map(|((driver, id), run)| async move {
                (
                    run,
                    inner.destroy_tracked(driver, &SandboxHandle { id }).await,
                )
            })
            .collect();
        while let Ok(Some((run, done))) = timeout_at(deadline, pending.next()).await {
            left.retain(|r| *r != run);
            match done {
                Ok(()) => {
                    // The run stays resumable; recovery writes its config afresh.
                    inner.remove_run_config(run);
                    report.destroyed.push(run);
                }
                Err(e) => report.failed.push((run, e.to_string())),
            }
        }
        drop(pending);
        // Travsr stops serving the runs stopped, in the grace period too. They stay resumable, so
        // their copies are kept.
        if let Some(indexing) = &inner.indexing {
            let mut releases: FuturesUnordered<_> = report
                .destroyed
                .iter()
                .map(|run| indexing.release(*run))
                .collect();
            while let Ok(Some(())) = timeout_at(deadline, releases.next()).await {}
        }
        // Best effort, in what is left of the grace period: the networks no sandbox uses any more.
        let unused: Vec<RunId> = {
            let live = inner.live();
            live.networks
                .keys()
                .filter(|run| {
                    !live.creating.contains(run) && !live.sandboxes.values().any(|r| r == *run)
                })
                .copied()
                .collect()
        };
        let mut releases: FuturesUnordered<_> = unused
            .into_iter()
            .map(|run| inner.release_network(run))
            .collect();
        while let Ok(Some(_)) = timeout_at(deadline, releases.next()).await {}
        report.timed_out.extend(left);
        report
    }

    /// Call once at startup, before `run_forever`.
    ///
    /// `Running` runs become `Suspended(Error("engine restarted"))`, `Provisioning` runs fail,
    /// `Suspended` runs stay suspended and `Queued` runs stay queued, except a run waiting on its
    /// own question (`kestrel.ask_human`): nobody waits for that answer any more, so the question is
    /// declined (unless already answered), the run fails, and its live children are cancelled
    /// (unless its spec keeps them). Sandboxes left behind by the
    /// previous engine are destroyed; a resumed run gets a fresh one. Then every leftover run
    /// network the drivers find is removed (best effort, like the sandboxes).
    ///
    /// Refused with [`EngineError::AlreadyRunning`] while `run_forever` is active.
    pub async fn recover(&self) -> Result<RecoveryReport> {
        if self.inner.live().active.is_some() {
            return Err(EngineError::AlreadyRunning);
        }
        let store = &self.inner.store;
        self.inner.sweep_leftovers().await;
        // After the sandboxes: docker refuses to remove a network a container still uses.
        self.inner.sweep_networks().await;
        let mut report = RecoveryReport::default();
        for run in store.list_runs(RunFilter::default())? {
            if run.status.is_terminal() {
                // An engine stopped between a run's end and its cleanup leaves its config behind.
                self.inner.remove_run_config(run.id);
                continue;
            }
            if run.status == RunStatus::Queued {
                continue;
            }
            self.inner.destroy_old_sandbox(run.id).await?;
            match run.status {
                RunStatus::Running => {
                    let next = RunStatus::Suspended(SuspendReason::Error(ENGINE_RESTARTED.into()));
                    store.transition(run.id, &RunStatus::Running, next)?;
                    report.interrupted.push(run.id);
                }
                RunStatus::Provisioning => {
                    let next = RunStatus::Failed(format!("{ENGINE_RESTARTED} while provisioning"));
                    store.transition(run.id, &RunStatus::Provisioning, next)?;
                }
                // A question (`kestrel.ask_human`) was asked from inside a tool call that is gone
                // now: nobody waits for the answer, and a resumed agent would start again from its
                // input. The question is declined and the run fails, saying why. (A suspended run
                // cannot move to another suspension, so it is not left resumable.)
                RunStatus::Suspended(SuspendReason::Approval(id))
                    if self.is_question(run.id, id)? =>
                {
                    // An answer written just before the stop stays the only decision.
                    if !self.is_resolved(run.id, id)? {
                        store.append(
                            run.id,
                            EventKind::ApprovalResolved {
                                id,
                                approved: false,
                                payload: Some(format!("{ENGINE_RESTARTED} before it was answered")),
                            },
                        )?;
                    }
                    // No orphans, as for any run that ends: its live children are cancelled when
                    // the engine starts, unless its spec keeps them. Queued before the run fails,
                    // so a stop in between leaves the run to this same path again.
                    let keeps = store.run_spec(run.id)?.is_some_and(|s| {
                        s.capabilities
                            .spawn
                            .as_ref()
                            .is_some_and(|p| p.keep_children)
                    });
                    if !keeps {
                        for below in store.subtree(run.id)? {
                            if !below.status.is_terminal() {
                                store.push_control(below.id, ControlCommand::Cancel)?;
                            }
                        }
                    }
                    let next = RunStatus::Failed(format!(
                        "{ENGINE_RESTARTED} while its question waited; run it again"
                    ));
                    store.transition(run.id, &run.status, next)?;
                }
                _ => report.suspended.push(run.id),
            }
        }
        report.travsr_unlisted = self.inner.forget_ended_indexes().await?;
        Ok(report)
    }
}

impl Inner {
    /// The slot pool. A poisoned lock still holds consistent data, so it is used as is.
    fn slots(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The sandbox registry and scheduler flag.
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks `run_forever` as active, with `stop` to end it, and starts with fresh slots.
    fn activate(&self, stop: &CancellationToken) -> Result<Active<'_>> {
        let mut live = self.live();
        if live.active.is_some() {
            return Err(EngineError::AlreadyRunning);
        }
        live.active = Some(stop.clone());
        drop(live);
        self.reset_slots();
        // Every slot is free again, so no parent is owed one: a request from before is stale (it
        // could hand a slot to a run whose sandbox the restart removed).
        self.rejoins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        let mut joins = self.joins.lock().unwrap_or_else(PoisonError::into_inner);
        joins.retain(|_, j| j.waiting > 0);
        for j in joins.values_mut() {
            j.released = false;
            j.regaining = false;
        }
        drop(joins);
        Ok(Active(self))
    }

    /// Every slot free again, as the actors that held them are gone, and so are the blocks they
    /// had reserved but not made a network on.
    fn reset_slots(&self) {
        *self.slots() = Slots::new(self.limits.max_concurrent_runs);
        let reserved: Vec<RunId> = {
            let live = self.live();
            live.blocks
                .keys()
                .filter(|run| !live.networks.contains_key(run))
                .copied()
                .collect()
        };
        for run in reserved {
            self.free_block(run);
        }
    }

    /// Resolves once `run_forever` is not active and no `create` is in flight.
    async fn settled(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let live = self.live();
                if live.active.is_none() && live.creating.is_empty() {
                    return;
                }
            }
            changed.await;
        }
    }

    /// Starts `create` as its own task, so it finishes (and the sandbox is registered) even if
    /// the run actor is aborted meanwhile.
    fn start_create(
        self: &Arc<Self>,
        run: RunId,
        driver: usize,
        launch: LaunchSpec,
    ) -> JoinHandle<kestrel_sandbox::Result<SandboxHandle>> {
        self.live().creating.insert(run);
        let inner = self.clone();
        tokio::spawn(async move {
            // Leaves `creating` when the task ends, also if the driver panics.
            let _creating = Creating { inner: &inner, run };
            let created = inner.drivers[driver].create(&launch).await;
            if let Ok(h) = &created {
                inner.live().sandboxes.insert((driver, h.id.clone()), run);
            }
            created
        })
    }

    /// Destroys the sandbox and, once that worked, drops it from the registry.
    async fn destroy_tracked(
        &self,
        driver: usize,
        h: &SandboxHandle,
    ) -> kestrel_sandbox::Result<()> {
        self.drivers[driver].destroy(h).await?;
        self.live().sandboxes.remove(&(driver, h.id.clone()));
        Ok(())
    }

    /// Best effort: destroys what the drivers list as left over (see `SandboxDriver::leftovers`).
    async fn sweep_leftovers(&self) {
        for (idx, driver) in self.drivers.iter().enumerate() {
            let Ok(found) = driver.leftovers().await else {
                continue;
            };
            for h in found {
                let _ = self.destroy_tracked(idx, &h).await;
            }
        }
    }

    /// Best effort: removes every leftover run network the drivers find (see
    /// `SandboxDriver::sweep_networks`). Every block this engine held goes back to the pool: the
    /// networks on them are gone, and docker refuses (then the engine excludes) any that is not.
    async fn sweep_networks(&self) {
        for driver in &self.drivers {
            let _ = driver.sweep_networks().await;
        }
        let blocks: Vec<RunId> = {
            let mut live = self.live();
            live.networks.clear();
            live.blocks.keys().copied().collect()
        };
        for run in blocks {
            self.free_block(run);
        }
    }

    /// Whether `run` needs a block before it can go on: the engine has a pool, the run holds no
    /// block, and it has no sandbox, so it is about to get a new one.
    fn needs_block(&self, run: RunId) -> bool {
        if self.subnets.is_none() {
            return false;
        }
        let live = self.live();
        !live.blocks.contains_key(&run) && !live.sandboxes.values().any(|r| *r == run)
    }

    /// The block `run` holds, reserving one if it holds none. `None` when the pool has no free
    /// block (or the engine has no pool).
    fn block_for(&self, run: RunId) -> Option<Subnet> {
        if let Some(block) = self.live().blocks.get(&run) {
            return Some(block.clone());
        }
        let block = self.subnets.as_ref()?.reserve()?;
        self.live().blocks.insert(run, block.clone());
        Some(block)
    }

    /// Gives `run`'s block back to the pool, if it holds one.
    fn free_block(&self, run: RunId) {
        let block = self.live().blocks.remove(&run);
        if let (Some(block), Some(pool)) = (block, &self.subnets) {
            pool.release(&block);
        }
    }

    /// Gives `run`'s block back if no network was made on it (a reservation not used).
    fn free_unused_block(&self, run: RunId) {
        if !self.live().networks.contains_key(&run) {
            self.free_block(run);
        }
    }

    /// Removes `run`'s network, if it has one, then frees its block. A network the driver could
    /// not remove retires its block (excluded until the next start), so the block is never handed
    /// out while the network may still use it, and it no longer counts as held by a run.
    async fn release_network(&self, run: RunId) -> kestrel_sandbox::Result<()> {
        let found = self.live().networks.remove(&run);
        let Some((driver, network)) = found else {
            self.free_block(run);
            return Ok(());
        };
        let released = self.drivers[driver].release_network(&network).await;
        if released.is_ok() {
            self.free_block(run);
        } else {
            let block = self.live().blocks.remove(&run);
            if let (Some(pool), Some(block)) = (&self.subnets, block) {
                pool.exclude(&[block.cidr()]);
            }
        }
        released
    }

    /// True when the subnet pool has no free block and no block held by a run that will come
    /// back: every block is excluded (taken by networks outside Kestrel, or retired because a
    /// network could not be removed) until the next start.
    fn pool_is_dead(&self) -> bool {
        self.subnets.as_ref().is_some_and(|p| p.exhausted())
    }

    /// Ends the wait of `run` for a block from a dead pool ([`Inner::pool_is_dead`]). A queued run
    /// (`cmd` is `None`) fails; for a resume or approve, the command is dropped with a log line
    /// naming it and the run stays suspended.
    fn give_up_on_pool(&self, run: RunId, cmd: Option<&ControlCommand>) -> Result<()> {
        let pool = self.subnets.as_ref().map(|p| p.pool().to_string());
        let why = format!(
            "no block of subnet pool {} can be used until kestrel serve restarts: every block is \
             taken by networks outside Kestrel or by run networks that could not be removed. Set \
             gateway.subnet_pool to a free range, or restart",
            pool.unwrap_or_default()
        );
        match cmd {
            Some(cmd) => {
                let what = match cmd {
                    c if c.approves() => "approve",
                    ControlCommand::Resume => "resume",
                    // Only resumes and approves wait for a slot (see `route`); anything else is
                    // named generically rather than mislabelled.
                    _ => "command",
                };
                let line = format!("kestrel: {what} not possible: {why}");
                self.store.append(run, EventKind::AgentLog { line })?;
            }
            None => {
                // The run may have been cancelled meanwhile; then there is nothing to fail. Any
                // other store error leaves the run `Queued`, so the next tick lines it up and
                // tries again.
                let _ = self
                    .store
                    .transition(run, &RunStatus::Queued, RunStatus::Failed(why));
            }
        }
        Ok(())
    }

    /// The line a run waiting for a block gets, once per wait.
    fn pool_note(&self) -> String {
        let pool = self.subnets.as_ref().map(|p| p.pool().to_string());
        format!(
            "kestrel: waiting for a network block: every /29 block of subnet pool {} is in use",
            pool.unwrap_or_default()
        )
    }

    /// One scheduler tick: forget finished actors, route control commands, line up the queued
    /// runs and the commands that need a slot, then hand out the free slots in that order.
    fn poll(self: &Arc<Self>, s: &mut Scheduler) -> Result<()> {
        while s.tasks.try_join_next().is_some() {}
        s.actors.retain(|_, tx| !tx.is_closed());

        let mut resumes = Vec::new();
        for (_, run, cmd) in self.store.take_control()? {
            let shown = format!("{cmd:?}");
            if let Err(e) = self.route(run, cmd, s, &mut resumes) {
                // One failing command must not stop the scheduler or the rest of the batch.
                let line = format!("kestrel: command {shown} failed: {e}");
                let _ = self.store.append(run, EventKind::AgentLog { line });
            }
        }
        self.cancel_ready(s, &mut resumes)?;
        self.line_up_queued(s)?;
        s.waiting.extend(resumes);
        for (run, rejoin) in self.take_rejoins() {
            s.waiting.push_back(Waiter {
                run,
                resume: None,
                told: false,
                rejoin: Some(rejoin),
            });
        }
        self.grant(s)
    }

    /// Asks for a cancel of `run` and of every live run below it. Each run's cancel is held back
    /// until its children have ended, so a tree is cancelled from the leaves up and every child's
    /// `Cancelled` is committed before its parent's.
    fn cancel_tree(self: &Arc<Self>, run: RunId, s: &mut Scheduler) -> Result<()> {
        if self.is_cancelling(run) {
            return Ok(());
        }
        // Read first, and hold the cancel back before going down: a store error on the way leaves
        // the run held back (`cancel_ready` looks below it again), never just marked.
        let below = self.live_below(run)?;
        self.cancelling_insert(run);
        s.pending_cancels.push(run);
        for run_below in below {
            self.cancel_tree(run_below, s)?;
        }
        Ok(())
    }

    /// Delivers the held-back cancels whose runs have no live child left. A child spawned since
    /// the cancel was asked for is cancelled too.
    fn cancel_ready(self: &Arc<Self>, s: &mut Scheduler, resumes: &mut Vec<Waiter>) -> Result<()> {
        let mut i = 0;
        while let Some(&run) = s.pending_cancels.get(i) {
            let live = self.live_below(run)?;
            for below in &live {
                self.cancel_tree(*below, s)?;
            }
            if !live.is_empty() {
                i += 1;
                continue;
            }
            s.pending_cancels.remove(i);
            self.cancelling_remove(run);
            s.waiting.retain(|w| !w.is_resume_of(run));
            resumes.retain(|w| !w.is_resume_of(run));
            if let Err(e) = self.deliver(run, ControlCommand::Cancel, s) {
                let line = format!("kestrel: command Cancel failed: {e}");
                let _ = self.store.append(run, EventKind::AgentLog { line });
            }
        }
        Ok(())
    }

    /// The nearest runs below `run` that have not ended: its live children, and below a child
    /// that ended (it may have kept its children), that child's nearest live runs.
    fn live_below(&self, run: RunId) -> Result<Vec<RunId>> {
        let mut live = Vec::new();
        let mut look = vec![run];
        while let Some(r) = look.pop() {
            for child in self.store.children(r)? {
                if child.status.is_terminal() {
                    look.push(child.id);
                } else {
                    live.push(child.id);
                }
            }
        }
        Ok(live)
    }

    fn cancelling(&self) -> MutexGuard<'_, HashSet<RunId>> {
        self.cancelling
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn cancelling_insert(&self, run: RunId) -> bool {
        self.cancelling().insert(run)
    }

    fn cancelling_remove(&self, run: RunId) {
        self.cancelling().remove(&run);
    }

    fn is_cancelling(&self, run: RunId) -> bool {
        self.cancelling().contains(&run)
    }

    /// Puts held-back cancels back into the store when the scheduler stops.
    fn requeue_cancels(&self, pending: &[RunId]) {
        for run in pending {
            self.cancelling_remove(*run);
            let _ = self.store.push_control(*run, ControlCommand::Cancel);
        }
    }

    /// A join of `parent` starts waiting. The first one gives the parent's slot back.
    fn enter_join(&self, parent: RunId) {
        let mut joins = self.joins.lock().unwrap_or_else(PoisonError::into_inner);
        let state = joins.entry(parent).or_default();
        state.waiting += 1;
        if state.waiting == 1 {
            if state.regaining {
                // The slot is still on its way back from an earlier join: this one takes it back.
                state.regaining = false;
                state.released = true;
            } else {
                state.released = self.slots().release(parent);
            }
        }
    }

    /// A join of `parent` stops waiting. True when it was the last one and the first gave the
    /// slot back, so the slot is to be taken again.
    fn leave_join(&self, parent: RunId) -> bool {
        let mut joins = self.joins.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(state) = joins.get_mut(&parent) else {
            return false;
        };
        state.waiting = state.waiting.saturating_sub(1);
        if state.waiting > 0 {
            return false;
        }
        if state.released {
            // Kept until the slot is back, so a join that starts meanwhile knows.
            state.released = false;
            state.regaining = true;
            return true;
        }
        joins.remove(&parent);
        false
    }

    /// Lines `run` up for its slot back after a join.
    fn rejoin(&self, run: RunId, rejoin: Rejoin) {
        self.rejoins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((run, rejoin));
    }

    fn take_rejoins(&self) -> Vec<(RunId, Rejoin)> {
        std::mem::take(&mut *self.rejoins.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Gives the waiter at `i`, a parent leaving a join, its slot back. False when no slot is
    /// free (nobody behind it can go either). A parent whose sandbox no longer runs (it ended, or
    /// was paused or suspended for budget) leaves the line with nothing; so does a told waiter
    /// whose join went away (it left a detached one instead). A parent waiting on an approval
    /// keeps its sandbox, so it gets its slot like a running one.
    fn grant_rejoin(&self, i: usize, s: &mut Scheduler) -> Result<bool> {
        let Some(w) = s.waiting.get(i) else {
            return Ok(true);
        };
        let run = w.run;
        let wanted = match &w.rejoin {
            Some(Rejoin::Told(tx)) => !tx.is_closed(),
            Some(Rejoin::Detached) => true,
            None => false,
        };
        let running = self.store.get_run(run)?.is_some_and(|r| {
            matches!(
                r.status,
                RunStatus::Running
                    | RunStatus::Provisioning
                    | RunStatus::Suspended(SuspendReason::Approval(_))
            )
        });
        // Joins before slots, the order `enter_join` takes them in.
        let mut joins = self.joins.lock().unwrap_or_else(PoisonError::into_inner);
        // A join of this parent waits again: it takes the slot back itself when it ends.
        let joining = joins.get(&run).is_some_and(|j| j.waiting > 0);
        if !(wanted && running) || joining {
            s.waiting.remove(i);
            if !running && !joining {
                joins.remove(&run);
            }
            return Ok(true);
        }
        if !self.slots().reacquire(run) {
            return Ok(false);
        }
        // The joins lock is still held, so no join can start until the state below is settled.
        let handed = match s.waiting.remove(i) {
            Some(Waiter {
                rejoin: Some(Rejoin::Told(tx)),
                ..
            }) => tx.send(()).is_ok(),
            _ => true,
        };
        if handed {
            // The slot is back: nothing is on its way any more.
            joins.remove(&run);
        } else {
            // The join went away just now. The slot is still owed: its detached waiter, or a join
            // that starts meanwhile, takes it back.
            self.slots().release(run);
            joins.entry(run).or_default().regaining = true;
        }
        Ok(true)
    }

    /// Delivers `cmd` now, or, when it resumes a run that holds no slot, adds it to `resumes`.
    fn route(
        self: &Arc<Self>,
        run: RunId,
        cmd: ControlCommand,
        s: &mut Scheduler,
        resumes: &mut Vec<Waiter>,
    ) -> Result<()> {
        if matches!(cmd, ControlCommand::Cancel) {
            s.waiting.retain(|w| !w.is_resume_of(run));
            resumes.retain(|w| !w.is_resume_of(run));
            // Children first: the run's own cancel goes out once its subtree has ended.
            return self.cancel_tree(run, s);
        }
        let needs_slot = cmd == ControlCommand::Resume || cmd.approves();
        if needs_slot && !self.slots().holds(run) && self.is_suspended(run)? {
            if s.waiting
                .iter()
                .chain(&*resumes)
                .any(|w| w.is_resume_of(run))
            {
                self.store.append(
                    run,
                    EventKind::AgentLog {
                        line: format!(
                            "kestrel: a resume already waits for a slot, {cmd:?} ignored"
                        ),
                    },
                )?;
            } else {
                resumes.push(Waiter {
                    run,
                    resume: Some(cmd),
                    told: false,
                    rejoin: None,
                });
            }
            return Ok(());
        }
        self.deliver(run, cmd, s)?;
        Ok(())
    }

    fn is_suspended(&self, run: RunId) -> Result<bool> {
        let row = self.store.get_run(run)?;
        Ok(row.is_some_and(|r| matches!(r.status, RunStatus::Suspended(_))))
    }

    /// Adds the `Queued` runs not yet waiting to the line, oldest first.
    fn line_up_queued(&self, s: &mut Scheduler) -> Result<()> {
        let known: HashSet<RunId> = s
            .waiting
            .iter()
            .filter(|w| w.resume.is_none() && w.rejoin.is_none())
            .map(|w| w.run)
            .collect();
        let queued = self.store.list_runs(RunFilter {
            status: Some(RunStatus::Queued),
            ..RunFilter::default()
        })?;
        for run in queued.into_iter().filter(|r| !known.contains(&r.id)) {
            s.waiting.push_back(Waiter {
                run: run.id,
                resume: None,
                told: false,
                rejoin: None,
            });
        }
        Ok(())
    }

    /// Gives free slots to the waiters, first come first served. When a waiter finds no slot,
    /// nobody behind it can: every waiter needs exactly one.
    ///
    /// A waiter that also needs a network block ([`Inner::needs_block`]) and finds none gives its
    /// slot back and stays in line, told once. Behind it, only waiters that need no block may go
    /// on: no block is free for the others either.
    fn grant(self: &Arc<Self>, s: &mut Scheduler) -> Result<()> {
        let mut i = 0;
        let mut starved = false;
        while let Some(waiter) = s.waiting.get_mut(i) {
            if waiter.rejoin.is_some() {
                if self.grant_rejoin(i, s)? {
                    continue;
                }
                break;
            }
            let (run, resume) = (waiter.run, waiter.resume.is_some());
            let needs_block = self.needs_block(run);
            if starved && needs_block {
                i += 1;
                continue;
            }
            let got = {
                let mut slots = self.slots();
                if resume {
                    slots.reacquire(run)
                } else {
                    slots.try_acquire(run)
                }
            };
            if !got {
                break;
            }
            if needs_block && self.block_for(run).is_none() {
                self.slots().release(run);
                if self.pool_is_dead() {
                    // Nothing is free and no run holds a block that will come back: waiting would
                    // never end. A queued run fails; a resume or approve is dropped and the run
                    // stays suspended.
                    let dropped = s.waiting.remove(i).and_then(|w| w.resume);
                    self.give_up_on_pool(run, dropped.as_ref())?;
                    continue;
                }
                starved = true;
                if !waiter.told {
                    waiter.told = true;
                    let line = self.pool_note();
                    self.store.append(run, EventKind::AgentLog { line })?;
                }
                i += 1;
                continue;
            }
            let Some(waiter) = s.waiting.remove(i) else {
                break;
            };
            let started = match waiter.resume {
                None => self.start_queued(run, s)?,
                Some(cmd) => self.start_resume(run, cmd, s)?,
            };
            if !started {
                self.slots().release(run);
                self.free_unused_block(run);
            }
        }
        Ok(())
    }

    /// Starts a queued run. False when it was cancelled while it waited.
    fn start_queued(self: &Arc<Self>, run: RunId, s: &mut Scheduler) -> Result<bool> {
        let claimed = self
            .store
            .transition(run, &RunStatus::Queued, RunStatus::Provisioning);
        if claimed.is_err() {
            return Ok(false);
        }
        let input = self
            .store
            .get_run(run)?
            .map(|r| r.input)
            .unwrap_or_default();
        self.spawn(run, RunStatus::Provisioning, input, s)?;
        Ok(s.actors.contains_key(&run))
    }

    /// Delivers a resume that got its slot. False when the run is no longer suspended.
    fn start_resume(
        self: &Arc<Self>,
        run: RunId,
        cmd: ControlCommand,
        s: &mut Scheduler,
    ) -> Result<bool> {
        Ok(self.is_suspended(run)? && self.deliver(run, cmd, s)?)
    }

    /// Sends `cmd` to the run's actor, or handles it here when there is none. True when an actor
    /// has the run afterwards.
    fn deliver(
        self: &Arc<Self>,
        run: RunId,
        cmd: ControlCommand,
        s: &mut Scheduler,
    ) -> Result<bool> {
        let cmd = match s.actors.get(&run) {
            Some(tx) => match tx.send(cmd) {
                Ok(()) => return Ok(true),
                // The actor finished since the last tick (and released its slot).
                Err(mpsc::error::SendError(cmd)) => cmd,
            },
            None => cmd,
        };
        self.idle_command(run, cmd, s)?;
        Ok(s.actors.contains_key(&run))
    }

    /// A command for a run that has no actor in this engine.
    fn idle_command(
        self: &Arc<Self>,
        run: RunId,
        cmd: ControlCommand,
        s: &mut Scheduler,
    ) -> Result<()> {
        let Some(row) = self.store.get_run(run)? else {
            return Ok(());
        };
        match (&row.status, cmd) {
            // Suspended before a restart: an actor takes it over from here.
            (RunStatus::Suspended(_), cmd) => {
                self.spawn(run, row.status.clone(), row.input, s)?;
                if let Some(tx) = s.actors.get(&run) {
                    // The actor was just created and holds its receiver.
                    let _ = tx.send(cmd);
                }
            }
            (RunStatus::Queued, ControlCommand::SetBudget(limit)) => {
                // The run starts later and reads its stored spec then.
                match self.store.set_budget(run, limit) {
                    Ok(_) => {}
                    Err(StoreError::ParentBudget { asked, remaining }) => {
                        let line = budget_refused(asked, remaining);
                        self.store.append(run, EventKind::AgentLog { line })?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            (RunStatus::Queued, ControlCommand::Cancel) => {
                self.store
                    .transition(run, &RunStatus::Queued, RunStatus::Cancelled)?;
            }
            (status, cmd) => {
                self.store.append(run, ignored(&cmd, status))?;
            }
        }
        Ok(())
    }

    /// Starts an actor for `run`, which is in `status`. Fails the run if its spec or gateway
    /// token is not stored (runs from before schema v3).
    fn spawn(
        self: &Arc<Self>,
        run: RunId,
        status: RunStatus,
        input: String,
        s: &mut Scheduler,
    ) -> Result<()> {
        let (Some(spec), Some(token)) = (self.store.run_spec(run)?, self.store.run_token(run)?)
        else {
            let failed =
                RunStatus::Failed("no agent spec or run token is stored for this run".into());
            self.store.transition(run, &status, failed)?;
            self.slots().release(run);
            return Ok(());
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Actor::new(self.clone(), run, spec, input, token, status, rx);
        s.tasks.spawn(actor.run());
        s.actors.insert(run, tx);
        Ok(())
    }

    /// An interval of `poll_interval` (at least 1ms) that does not burst after a stall.
    fn ticker(&self) -> Interval {
        let period = self.limits.poll_interval.max(Duration::from_millis(1));
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tick
    }

    /// The driver for `tier`: the first one for `Auto`, else the one with that name.
    fn driver_for(&self, tier: SandboxTier) -> Option<usize> {
        let name = match tier {
            SandboxTier::Auto => return (!self.drivers.is_empty()).then_some(0),
            SandboxTier::Gvisor => "gvisor",
            SandboxTier::AppleContainer => "apple-container",
            SandboxTier::DockerRunc => "docker-runc",
        };
        self.drivers.iter().position(|d| d.name() == name)
    }

    /// The gateway URL for a sandbox on `network`: the configured one, with its host replaced by
    /// the bridge's address on the run's own network when there is one.
    fn gateway_for(&self, network: Option<&PreparedNetwork>) -> Url {
        let mut url = self.gateway_url.clone();
        if let Some(net) = network {
            // Only a URL that cannot have a host refuses this; it then stays as configured.
            let _ = url.set_ip_host(IpAddr::V4(net.gateway_host));
        }
        url
    }

    /// The provider model name for `tier`.
    fn model(&self, tier: Tier) -> String {
        self.models
            .get(&tier)
            .cloned()
            .unwrap_or_else(|| default_model(tier).to_string())
    }

    /// The config directory of `run`.
    fn run_config_dir(&self, run: RunId) -> PathBuf {
        self.run_config.join(run.to_string())
    }

    /// Writes `files` to `run`'s config directory, made afresh in the owner-only root (see
    /// [`Engine::with_run_config`]). `None` when there are no files. A name that is not a plain
    /// file name is refused, so nothing is written outside the directory.
    fn write_run_config(
        &self,
        run: RunId,
        files: &[kestrel_agent::PlanFile],
    ) -> Result<Option<PathBuf>> {
        self.remove_run_config(run);
        if files.is_empty() {
            return Ok(None);
        }
        for file in files {
            let plain = !file.name.is_empty()
                && file.name != "."
                && file.name != ".."
                && !file.name.contains(['/', '\\', '\0']);
            if !plain {
                return Err(EngineError::RunConfig(format!(
                    "plan file name `{}` is not a plain file name",
                    file.name
                )));
            }
        }
        let dir = self.run_config_dir(run);
        let failed = |e: std::io::Error| EngineError::RunConfig(format!("{}: {e}", dir.display()));
        std::fs::create_dir_all(&self.run_config).map_err(failed)?;
        // Never through a link: the root is what keeps other host users out, so it must be the
        // directory itself.
        let root = std::fs::symlink_metadata(&self.run_config).map_err(failed)?;
        if !root.is_dir() {
            return Err(EngineError::RunConfig(format!(
                "{}: not a directory (a link is refused)",
                self.run_config.display()
            )));
        }
        // Someone else's directory would stay theirs to open again, whatever mode is set here.
        if !owned_by_us(&self.run_config, &root).map_err(failed)? {
            return Err(EngineError::RunConfig(format!(
                "{}: owned by another user",
                self.run_config.display()
            )));
        }
        // Also when it existed already.
        set_mode(&self.run_config, 0o700).map_err(failed)?;
        // Made private, written, then opened to any user. Each mode is set explicitly, so a
        // strict umask cannot leave the files unreadable to the sandbox's user.
        std::fs::create_dir(&dir).map_err(failed)?;
        set_mode(&dir, 0o700).map_err(failed)?;
        for file in files {
            let path = dir.join(&file.name);
            let written = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, file.contents.as_bytes()));
            written.map_err(failed)?;
            set_mode(&path, 0o644).map_err(failed)?;
        }
        set_mode(&dir, 0o755).map_err(failed)?;
        Ok(Some(dir))
    }

    /// Removes `run`'s config directory, if there is one (best effort). Like writing, never
    /// through a root that is a link. The owner is not checked here, and a root swapped for a link
    /// between the check and the removal is not caught. That is accepted because the root is
    /// meant to sit where no other user can rename or replace it (the default one does: it is
    /// ours, in the sticky temp dir); nothing checks the parent of a configured root.
    fn remove_run_config(&self, run: RunId) {
        let real_dir = std::fs::symlink_metadata(&self.run_config).is_ok_and(|m| m.is_dir());
        if real_dir {
            let _ = std::fs::remove_dir_all(self.run_config_dir(run));
        }
    }

    /// Best effort: destroys the last sandbox a previous engine provisioned for `run`, and its
    /// config directory.
    async fn destroy_old_sandbox(&self, run: RunId) -> Result<()> {
        let old = self
            .store
            .events_since(run, 0)?
            .into_iter()
            .rev()
            .find_map(|e| match e.kind {
                EventKind::SandboxProvisioned { driver, sandbox_id } => Some((driver, sandbox_id)),
                _ => None,
            });
        if let Some((name, id)) = old
            && let Some(idx) = self.drivers.iter().position(|d| d.name() == name)
        {
            // The sandbox may well be gone already.
            let _ = self.destroy_tracked(idx, &SandboxHandle { id }).await;
        }
        self.remove_run_config(run);
        // Its Travsr copy stays: a resumed run is served from it again.
        Ok(())
    }

    /// Forgets `run`'s Travsr copy. A failure is written to the run's log and tried again later
    /// (see [`Engine::with_indexer`]).
    async fn forget_index(&self, run: RunId) {
        let Some(indexing) = &self.indexing else {
            return;
        };
        if let Err(e) = indexing.forget(run).await {
            let line = format!(
                "kestrel: Travsr could not forget this run's copy, it will be retried: {e}"
            );
            // A run that no longer exists has no log; the retry still runs.
            let _ = self.store.append(run, EventKind::AgentLog { line });
        }
    }

    /// Forgets every Travsr copy whose run ended or no longer exists, a few at a time, then
    /// retries the forgets that failed before. For [`Engine::recover`]: copies are kept by run
    /// id, so a fresh engine finds them all. `Some` says why the copies could not be listed.
    async fn forget_ended_indexes(&self) -> Result<Option<String>> {
        let Some(indexing) = &self.indexing else {
            return Ok(None);
        };
        let (kept, unlisted) = match indexing.kept().await {
            Ok(kept) => (kept, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let mut ended = Vec::new();
        for run in kept {
            let gone = match self.store.get_run(run)? {
                Some(row) => row.status.is_terminal(),
                None => true,
            };
            if gone {
                ended.push(run);
            }
        }
        futures::stream::iter(ended)
            .for_each_concurrent(4, |run| self.forget_index(run))
            .await;
        indexing.retry().await;
        Ok(unlisted)
    }
}

/// A `create` in flight for `run`; dropping it ends that.
struct Creating<'a> {
    inner: &'a Inner,
    run: RunId,
}

impl Drop for Creating<'_> {
    fn drop(&mut self) {
        self.inner.live().creating.remove(&self.run);
        self.inner.changed.notify_waiters();
    }
}

/// Marks `run_forever` as active; resets the slots and clears the flag when it ends.
struct Active<'a>(&'a Inner);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.reset_slots();
        self.0.live().active = None;
        self.0.changed.notify_waiters();
    }
}

/// The per-run gateway URL, `{gateway}/r/{token}`.
fn run_gateway(gateway: &Url, token: &str) -> Url {
    let mut url = gateway.clone();
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().push("r").push(token);
    }
    url
}

/// An `AgentLog` noting that `cmd` does not apply to a run in `status`.
fn ignored(cmd: &ControlCommand, status: &RunStatus) -> EventKind {
    EventKind::AgentLog {
        line: format!("kestrel: ignored {cmd:?} while the run is {status:?}"),
    }
}

/// A run's indexing job, see [`Actor::start_index`].
type IndexJob =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<(), String>> + Send>>;

/// What `job` gives, or never when there is none.
async fn some(job: &mut Option<IndexJob>) -> std::result::Result<(), String> {
    match job {
        Some(job) => job.await,
        None => std::future::pending().await,
    }
}

/// A sandbox this run created, and which driver owns it.
#[derive(Clone)]
struct Sandbox {
    driver: usize,
    handle: SandboxHandle,
}

/// What the current phase of a run does after an input.
enum Next {
    Continue,
    /// Leave the phase: the run is suspended or finished.
    Stop,
    /// A resume was accepted.
    Resume,
}

/// The last `Finished` the agent reported: `(ok, summary)`.
type Finished = Option<(bool, Option<String>)>;

/// Drives one run. The only writer of that run's status while it lives.
struct Actor {
    inner: Arc<Inner>,
    id: RunId,
    spec: AgentSpec,
    input: String,
    /// Gateway token of this run, see [`Store::run_token`].
    token: String,
    status: RunStatus,
    adapter: Box<dyn RuntimeAdapter>,
    sandbox: Option<Sandbox>,
    /// The limit the last 80% budget warning was written for. A larger limit gets its own
    /// warning; the same one (a limit that dropped for a spawn and came back) does not.
    warned_at: Option<MicroUsd>,
    clock: WallClock,
    /// The question (`kestrel.ask_human`) this run asked last. While the run waits on it, its
    /// sandbox keeps working, so its wall clock keeps going.
    asking: Option<ApprovalId>,
    cmds: mpsc::UnboundedReceiver<ControlCommand>,
}

impl Actor {
    fn new(
        inner: Arc<Inner>,
        id: RunId,
        spec: AgentSpec,
        input: String,
        token: String,
        status: RunStatus,
        cmds: mpsc::UnboundedReceiver<ControlCommand>,
    ) -> Actor {
        let adapter = (inner.adapters)(spec.runtime);
        let mut clock = WallClock::new(spec.budget.wall_clock);
        if status == RunStatus::Provisioning {
            clock.run();
        }
        Actor {
            inner,
            id,
            spec,
            input,
            token,
            status,
            adapter,
            sandbox: None,
            warned_at: None,
            clock,
            asking: None,
            cmds,
        }
    }

    /// Runs to a terminal status, then destroys the sandbox. An error fails the run.
    async fn run(mut self) {
        if let Err(e) = self.drive().await
            && !self.status.is_terminal()
        {
            // Nothing more to do if the store refuses this too.
            let _ = self.set(RunStatus::Failed(self.scrub(&e.to_string())));
        }
        // However the run ended, it leaves no child behind (a no-op when they ended already).
        let _ = self.reap_children().await;
        if let Some(sb) = self.sandbox.take() {
            let _ = self.inner.destroy_tracked(sb.driver, &sb.handle).await;
        }
        // The sandbox is gone, so its config goes too (a resumed run writes it again).
        self.inner.remove_run_config(self.id);
        // After the sandbox: docker refuses to remove a network a container still uses.
        if let Err(e) = self.inner.release_network(self.id).await {
            let _ = self.log(&format!("kestrel: removing the run network failed: {e}"));
        }
        // The run ended; whatever slot it still holds goes back.
        self.inner.slots().release(self.id);
        // With the sandbox gone, the clone holds what the agent left: its branch comes home
        // (#93). After the slot, so a slow fetch never holds one; never changes the outcome.
        if self.status.is_terminal()
            && let Some(line) = self.inner.bring_branch_home(self.id, &self.spec).await
        {
            let _ = self.log(&self.scrub(&line));
        }
        // Last, so a slow Travsr never holds the slot: a run that ended is forgotten, one that
        // did not (its status could not be stored) only stops being served.
        if self.status.is_terminal() {
            self.inner.forget_index(self.id).await;
        } else if let Some(indexing) = &self.inner.indexing {
            indexing.release(self.id).await;
        }
    }

    /// Cancels the children still going and waits until they have ended, unless the spec says to
    /// keep them (`spawn.keep_children`).
    async fn reap_children(&mut self) -> Result<()> {
        let keep = self
            .spec
            .capabilities
            .spawn
            .as_ref()
            .is_some_and(|s| s.keep_children);
        if keep {
            return Ok(());
        }
        let live = self.inner.live_below(self.id)?;
        if live.is_empty() {
            return Ok(());
        }
        self.log(&format!(
            "kestrel: cancelling {} child run(s) still going",
            live.len()
        ))?;
        for child in live {
            self.inner
                .store
                .push_control(child, ControlCommand::Cancel)?;
        }
        let period = self
            .inner
            .limits
            .poll_interval
            .max(Duration::from_millis(1));
        while !self.inner.live_below(self.id)?.is_empty() {
            tokio::time::sleep(period).await;
        }
        Ok(())
    }

    /// Whether the run's sandbox is running or being provisioned, which is what a slot pays for.
    /// An approval wait after a restart has no sandbox.
    fn holds_slot(&self) -> bool {
        let approval = matches!(
            self.status,
            RunStatus::Suspended(SuspendReason::Approval(_))
        );
        matches!(self.status, RunStatus::Provisioning | RunStatus::Running)
            || (approval && self.sandbox.is_some())
    }

    /// Releases the slot if the run's state no longer needs one.
    fn sync_slot(&self) {
        if !self.holds_slot() {
            self.inner.slots().release(self.id);
        }
    }

    /// Makes sure the run holds a slot before it starts a sandbox. The scheduler normally took
    /// one already; this also covers a command that raced with a suspension.
    fn ensure_slot(&self) -> Result<bool> {
        if self.inner.slots().reacquire(self.id) {
            return Ok(true);
        }
        self.log("kestrel: no free slot, command ignored; send it again")?;
        Ok(false)
    }

    async fn drive(&mut self) -> Result<()> {
        let mut go = self.status == RunStatus::Provisioning;
        while !self.status.is_terminal() {
            if !go {
                go = self.wait_for_resume().await?;
                continue;
            }
            go = false;
            let sandbox = match self.sandbox.clone() {
                Some(sb) => Some(sb),
                None => self.provision().await?,
            };
            if let Some(sb) = sandbox {
                self.attend(sb).await?;
            }
        }
        Ok(())
    }

    /// While suspended: applies one command. True when the run should run again.
    async fn wait_for_resume(&mut self) -> Result<bool> {
        let cmd = recv(&mut self.cmds).await;
        // Only these may have been given a slot (and a block) by the scheduler. Any other command
        // leaves both alone: a resume it arrived with may be right behind it, and its slot and
        // block must still be there when it is read.
        let resumes = cmd == ControlCommand::Resume || cmd.approves();
        let next = self.on_command(cmd)?;
        // An approval granted after a restart leaves the run `Running` with nothing attached.
        let go = matches!(next, Next::Resume) || self.status == RunStatus::Running;
        if !go && resumes {
            // A resume that was declined: nothing here needs the slot (or a block) taken for it.
            // A cancel or a reject ends the run, which gives both back.
            self.sync_slot();
            self.inner.free_unused_block(self.id);
        }
        Ok(go)
    }

    /// Creates the sandbox, applying commands while `create` runs, and has the run's clone
    /// indexed for Travsr meanwhile (see [`Engine::with_indexer`]); the agent starts once both
    /// are done. `None` when the run ended meanwhile (the sandbox, if created, is destroyed on
    /// exit).
    async fn provision(&mut self) -> Result<Option<Sandbox>> {
        let inner = self.inner.clone();
        let idx = inner
            .driver_for(self.spec.sandbox.tier)
            .ok_or(EngineError::NoDriver)?;
        let driver = &inner.drivers[idx];
        // The network first: the launch plan holds the bridge's address on it.
        let network = self.prepare_network(idx).await?;
        let mut launch = self.launch_spec(network.as_ref())?;
        let (workspace, fresh) = self.workspace().await?;
        launch.workspace = workspace;
        // Before the sandbox exists: a fresh workspace's commit is read now or never.
        let mut index = self.start_index(fresh).await;
        let mut create = inner.start_create(self.id, idx, launch);
        let created = loop {
            let deadline = self.clock.deadline();
            tokio::select! {
                res = &mut create => break res.unwrap_or_else(|e| Err(create_died(&e))),
                done = some(&mut index) => {
                    index = None;
                    self.indexed(done)?;
                }
                cmd = recv(&mut self.cmds) => {
                    self.on_command(cmd)?;
                }
                _ = until(deadline), if !self.status.is_terminal() => {
                    self.set(RunStatus::Failed(WALL_CLOCK_EXCEEDED.into()))?;
                }
            }
        };
        let sb = Sandbox {
            driver: idx,
            handle: created?,
        };
        self.sandbox = Some(sb.clone());
        if self.status.is_terminal() {
            return Ok(None);
        }
        self.inner.store.append(
            self.id,
            EventKind::SandboxProvisioned {
                driver: driver.name().into(),
                sandbox_id: sb.handle.id.clone(),
            },
        )?;
        // The agent waits for its clone's index, at most the indexing timeout.
        while let Some(job) = index.as_mut()
            && !self.status.is_terminal()
        {
            let deadline = self.clock.deadline();
            tokio::select! {
                done = job => {
                    index = None;
                    self.indexed(done)?;
                }
                cmd = recv(&mut self.cmds) => {
                    self.on_command(cmd)?;
                }
                _ = until(deadline) => {
                    self.set(RunStatus::Failed(WALL_CLOCK_EXCEEDED.into()))?;
                }
            }
        }
        if self.status.is_terminal() {
            return Ok(None);
        }
        Ok(Some(sb))
    }

    /// The indexing of this run's code, when the engine has an indexer and workspaces, the
    /// workspace is a `git-clone` and the run may call a Travsr tool. Dropping it gives the job
    /// up.
    ///
    /// The indexer gets the source repo, never the workspace. For a `fresh` workspace (cloned
    /// just now, its sandbox not made yet) it also gets the commit the workspace is at; a reused
    /// one is the agent's, so git never runs in it and the indexer uses the commit it recorded.
    async fn start_index(&self, fresh: bool) -> Option<IndexJob> {
        let indexing = self.inner.indexing.clone()?;
        let workspaces = self.inner.workspaces.as_ref()?;
        let WorkspaceSpec::GitClone { repo, .. } = &self.spec.sandbox.workspace else {
            return None;
        };
        if !index::wants_travsr(&self.spec) {
            return None;
        }
        let commit = if fresh {
            match workspaces.head(self.id).await {
                Ok(commit) => Some(commit),
                Err(e) => {
                    let why = format!("the workspace's commit could not be read: {e}");
                    return Some(Box::pin(std::future::ready(Err(why))));
                }
            }
        } else {
            None
        };
        let source = IndexSource {
            repo: repo.clone(),
            commit,
        };
        let run = self.id;
        Some(Box::pin(async move { indexing.index(run, source).await }))
    }

    /// Notes how indexing ended: a run whose clone is not indexed starts without Travsr, and its
    /// log says why.
    fn indexed(&self, done: std::result::Result<(), String>) -> Result<()> {
        if let Err(why) = done {
            self.log(&format!(
                "kestrel: Travsr is unavailable for this run ({why}); the agent starts without it"
            ))?;
        }
        Ok(())
    }

    /// Prepares this run's workspace, if the engine has workspaces. Reuses an existing one, so
    /// a resumed run gets the same directory back. `true` with a workspace made just now.
    async fn workspace(&self) -> Result<(Option<PathBuf>, bool)> {
        let Some(workspaces) = &self.inner.workspaces else {
            return Ok((None, false));
        };
        let spec = &self.spec.sandbox.workspace;
        if let WorkspaceSpec::GitClone { repo, .. } = spec
            && !repo.is_absolute()
        {
            return Err(EngineError::RelativeRepo { repo: repo.clone() });
        }
        // Only this run's actor makes its workspace, so nothing makes it in between.
        let existed = tokio::fs::symlink_metadata(workspaces.path_for(self.id))
            .await
            .is_ok();
        let path = workspaces.prepare(self.id, spec).await?;
        Ok((Some(path), !existed))
    }

    /// The run's own network on a block of the engine's pool, when the engine has one and the
    /// driver makes run networks. Registered with the engine so it is removed when the run ends
    /// (or by `shutdown`). A run that already has a network reuses it. A block docker says is
    /// taken is excluded from the pool together with every block the driver's
    /// `taken_subnets` reports in use, and another one is tried, until the pool has no free
    /// block left ([`EngineError::NoSubnet`]); any other failure frees the block and fails the
    /// run.
    async fn prepare_network(&mut self, idx: usize) -> Result<Option<PreparedNetwork>> {
        let inner = self.inner.clone();
        let Some(pool) = &inner.subnets else {
            return Ok(None);
        };
        // A run that has a network keeps it: its stopped sandbox outlived the actor that made
        // it (`run_forever` stopped and started again without `recover`). The run's spec picks
        // the same driver, and docker would refuse a second network of that name.
        let existing = inner.live().networks.get(&self.id).cloned();
        if let Some((_, net)) = existing {
            return Ok(Some(net));
        }
        let driver = &inner.drivers[idx];
        // Blocks this run found taken, for the message if none is left.
        let mut taken_blocks = 0;
        // Every taken block is excluded, so each attempt leaves one block fewer and this ends
        // when the pool has none free. The bound is only a safety net.
        for _ in 0..pool.capacity() {
            let Some(subnet) = inner.block_for(self.id) else {
                break;
            };
            match driver.prepare_network(self.id, &subnet).await {
                Ok(Some(net)) => {
                    inner.live().networks.insert(self.id, (idx, net.clone()));
                    return Ok(Some(net));
                }
                Ok(None) => {
                    inner.free_block(self.id);
                    return Ok(None);
                }
                Err(SandboxError::SubnetTaken { subnet: taken }) => {
                    // Counted even if another run excluded it meanwhile: docker refused it.
                    taken_blocks += pool.exclude(&[taken]).max(1);
                    taken_blocks += self.exclude_listed(pool, driver.as_ref()).await;
                    inner.free_block(self.id);
                }
                Err(e) => {
                    inner.free_block(self.id);
                    return Err(e.into());
                }
            }
        }
        Err(EngineError::NoSubnet {
            pool: pool.pool(),
            taken: taken_blocks,
            held: pool.in_use(),
        })
    }

    /// Best effort: excludes from `pool` every block the subnets the driver reports in use
    /// (`SandboxDriver::taken_subnets`) cover, except the blocks other runs of this engine hold
    /// ([`SubnetPool::exclude_except`]). A held block stays theirs: one under a network made
    /// outside Kestrel is excluded once its run gives it back, and a subnet inside one held
    /// block (that run's own network, listed by a driver without an instance id) is ignored.
    /// Returns how many blocks were newly found taken. An error lists nothing.
    ///
    /// Known limit, only without an instance id (`serve` always sets one): the listing is read
    /// before the held blocks. If another run removes its network and frees its block in between,
    /// that run's /29 is still listed but no longer held, so the block is excluded until the next
    /// start. Conservative: a block is lost, never handed out twice.
    async fn exclude_listed(&self, pool: &SubnetPool, driver: &dyn SandboxDriver) -> usize {
        let Ok(listed) = driver.taken_subnets().await else {
            return 0;
        };
        let held: Vec<Subnet> = self
            .inner
            .live()
            .blocks
            .iter()
            .filter(|(run, _)| **run != self.id)
            .map(|(_, block)| block.clone())
            .collect();
        pool.exclude_except(&listed, &held)
    }

    fn launch_spec(&self, network: Option<&PreparedNetwork>) -> Result<LaunchSpec> {
        let gateway = self.inner.gateway_for(network);
        let ctx = RunContext {
            run_id: self.id,
            gateway_url: run_gateway(&gateway, &self.token),
            model: self.inner.model(self.spec.model_policy.default),
        };
        let plan = self.adapter.launch_plan(&self.spec, &self.input, &ctx)?;
        let config = self.inner.write_run_config(self.id, &plan.files)?;
        let mut labels = BTreeMap::from([(RUN_LABEL.to_string(), self.id.to_string())]);
        if let Some(instance) = &self.inner.instance {
            labels.insert(INSTANCE_LABEL.to_string(), instance.clone());
        }
        Ok(LaunchSpec {
            image: plan.image,
            argv: plan.argv,
            env: plan.env,
            workspace: None,
            config,
            cpu_millis: self.spec.sandbox.cpu_millis,
            memory_mb: self.spec.sandbox.memory_mb,
            network: match network {
                Some(net) => NetworkPolicy::RunNetwork {
                    network: net.name.clone(),
                    gateway,
                },
                None => NetworkPolicy::GatewayOnly { gateway },
            },
            labels,
        })
    }

    /// Starts the agent and follows it until it exits, the run is suspended, or the run ends.
    /// A suspended run's sandbox is stopped; its exit is not the run's outcome.
    async fn attend(&mut self, sb: Sandbox) -> Result<()> {
        let inner = self.inner.clone();
        let driver = &inner.drivers[sb.driver];
        let mut output = driver.start(&sb.handle).await?;
        if self.status != RunStatus::Running {
            self.set(RunStatus::Running)?;
        }
        let mut finished: Finished = None;
        let mut tick = inner.ticker();
        loop {
            let deadline = self.clock.deadline();
            let next = tokio::select! {
                line = output.next() => match line {
                    Some(line) => self.on_line(&line.line, &mut finished)?,
                    None => {
                        // Output read to the end: the exit code is final.
                        let exit = driver.wait(&sb.handle).await?;
                        // No orphans: children still going are cancelled first, so the run's
                        // `RunFinished` comes after theirs.
                        self.reap_children().await?;
                        return self.finish(finished, exit.code);
                    }
                },
                cmd = recv(&mut self.cmds) => self.on_command(cmd)?,
                _ = tick.tick() => self.check_budget()?,
                _ = until(deadline) => {
                    self.set(RunStatus::Failed(WALL_CLOCK_EXCEEDED.into()))?;
                    Next::Stop
                }
            };
            if matches!(next, Next::Stop) {
                break;
            }
        }
        if !self.status.is_terminal() {
            driver.stop(&sb.handle).await?;
        }
        Ok(())
    }

    /// `text` without this run's gateway token, which only the gateway may see.
    fn scrub(&self, text: &str) -> String {
        match self.token.as_str() {
            "" => text.to_string(),
            token => text.replace(token, "[token]"),
        }
    }

    fn on_line(&mut self, line: &str, finished: &mut Finished) -> Result<Next> {
        match self.adapter.parse_line(&self.scrub(line)) {
            ParsedLine::Ignore => {}
            ParsedLine::Finished { ok, summary } => *finished = Some((ok, summary)),
            ParsedLine::Events(events) => {
                for event in events {
                    self.record(event)?;
                }
            }
        }
        // The gateway writes model calls into the store as they happen.
        self.check_budget()
    }

    fn record(&mut self, event: EventKind) -> Result<()> {
        let approval = match &event {
            EventKind::ApprovalRequested { id, .. } => Some(*id),
            _ => None,
        };
        self.inner.store.append(self.id, event)?;
        if let Some(id) = approval
            && self.status == RunStatus::Running
        {
            // The sandbox keeps running while the approval is pending.
            self.set(RunStatus::Suspended(SuspendReason::Approval(id)))?;
        }
        Ok(())
    }

    fn on_command(&mut self, cmd: ControlCommand) -> Result<Next> {
        use ControlCommand as C;
        let pending = match &self.status {
            RunStatus::Suspended(SuspendReason::Approval(id)) => Some(*id),
            _ => None,
        };
        match cmd {
            C::Cancel if !self.status.is_terminal() => {
                self.set(RunStatus::Cancelled)?;
                Ok(Next::Stop)
            }
            C::SetBudget(limit) => {
                // Stored first, so the new limit survives a restart.
                match self.inner.store.set_budget(self.id, limit) {
                    Ok(set) => {
                        self.spec.budget.limit = set;
                        // A limit a human set gets its own warning.
                        self.warned_at = None;
                    }
                    Err(StoreError::ParentBudget { asked, remaining }) => {
                        self.log(&budget_refused(asked, remaining))?;
                    }
                    Err(e) => return Err(e.into()),
                }
                Ok(Next::Continue)
            }
            C::Pause if self.status == RunStatus::Running => {
                self.set(RunStatus::Suspended(SuspendReason::Manual))?;
                Ok(Next::Stop)
            }
            C::Resume if matches!(self.status, RunStatus::Suspended(_)) && pending.is_none() => {
                let over = self.status == RunStatus::Suspended(SuspendReason::Budget)
                    && self.spent()? >= self.inner.store.limit(self.id)?;
                if over {
                    self.log("kestrel: not resumed, the run is still over its budget")?;
                    return Ok(Next::Continue);
                }
                if !self.ensure_slot()? {
                    return Ok(Next::Continue);
                }
                Ok(Next::Resume)
            }
            cmd @ (C::Approve(_) | C::Reject(_) | C::ApproveWith { .. } | C::RejectWith { .. }) => {
                self.on_approval(cmd, pending)
            }
            C::Ask { id, question } => {
                if self.status == RunStatus::Running {
                    self.asking = Some(id);
                    self.record(EventKind::ApprovalRequested {
                        id,
                        action: question,
                        question: true,
                    })?;
                } else {
                    // The run stopped running before it got here: the asker hears no.
                    self.resolve(id, false, Some("the run is not running".into()))?;
                }
                Ok(Next::Continue)
            }
            other => {
                self.inner
                    .store
                    .append(self.id, ignored(&other, &self.status))?;
                Ok(Next::Continue)
            }
        }
    }

    /// Approve or reject, with or without text. Only the approval the run waits on counts.
    fn on_approval(&mut self, cmd: ControlCommand, pending: Option<ApprovalId>) -> Result<Next> {
        use ControlCommand as C;
        let Some(id) = cmd.approval().filter(|id| pending == Some(*id)) else {
            let id = cmd.approval().map_or_else(String::new, |id| id.to_string());
            self.log(&format!(
                "kestrel: no pending approval {id}, command ignored"
            ))?;
            return Ok(Next::Continue);
        };
        let (approved, text) = match cmd {
            C::ApproveWith { payload, .. } => (true, Some(payload)),
            C::RejectWith { reason, .. } => (false, Some(reason)),
            other => (other.approves(), None),
        };
        if approved {
            if !self.ensure_slot()? {
                return Ok(Next::Continue);
            }
            self.resolve(id, true, text)?;
            self.set(RunStatus::Running)?;
            return Ok(Next::Continue);
        }
        let failure = match &text {
            Some(reason) => format!("approval {id} rejected: {reason}"),
            None => format!("approval {id} rejected"),
        };
        self.resolve(id, false, text)?;
        self.set(RunStatus::Failed(failure))?;
        Ok(Next::Stop)
    }

    fn resolve(&self, id: ApprovalId, approved: bool, payload: Option<String>) -> Result<()> {
        self.inner.store.append(
            self.id,
            EventKind::ApprovalResolved {
                id,
                approved,
                payload,
            },
        )?;
        Ok(())
    }

    /// While `Running`: one `BudgetWarning` at 80% of the limit, `Suspended(Budget)` at 100%.
    fn check_budget(&mut self) -> Result<Next> {
        if self.status != RunStatus::Running {
            return Ok(Next::Continue);
        }
        let spent = self.spent()?;
        // Read from the store, not the spec this actor started with: spawning a child lowers the
        // limit, and a finished child's unspent budget raises it again.
        let mut limit = self.inner.store.limit(self.id)?;
        // A parent with children still going may have handed out all of its own budget: it cannot
        // spend (the gateway refuses it at its limit), but it stays up to wait for them. With none
        // left, read the limit again: the last child's refund commits with its end, so a child that
        // ended after the first read has already given its budget back.
        if spent >= limit && self.inner.store.live_children(self.id)? == 0 {
            limit = self.inner.store.limit(self.id)?;
            if spent >= limit {
                self.set(RunStatus::Suspended(SuspendReason::Budget))?;
                return Ok(Next::Stop);
            }
        }
        let new_limit = self.warned_at.is_none_or(|at| limit > at);
        if new_limit && i128::from(spent.0) * 5 >= i128::from(limit.0) * 4 {
            self.warned_at = Some(limit);
            self.inner
                .store
                .append(self.id, EventKind::BudgetWarning { spent, limit })?;
        }
        Ok(Next::Continue)
    }

    /// The outcome once the agent's output ended: the last `Finished` plus the exit code.
    fn finish(&mut self, finished: Finished, code: i32) -> Result<()> {
        match (finished, code) {
            (Some((true, summary)), 0) => self.complete(summary),
            (None, 0) => self.complete(None),
            (Some((false, summary)), _) => {
                let msg = summary.unwrap_or_else(|| format!("agent failed (exit code {code})"));
                self.set(RunStatus::Failed(msg))
            }
            _ => self.set(RunStatus::Failed(format!("agent exited with code {code}"))),
        }
    }

    fn complete(&mut self, summary: Option<String>) -> Result<()> {
        if self.status != RunStatus::Running {
            // The agent finished while an approval was pending.
            self.set(RunStatus::Running)?;
        }
        self.inner
            .store
            .append(self.id, EventKind::RunFinished { summary })?;
        self.set(RunStatus::Completed)
    }

    fn spent(&self) -> Result<MicroUsd> {
        Ok(self.inner.store.spent(self.id)?)
    }

    fn log(&self, line: &str) -> Result<()> {
        self.inner
            .store
            .append(self.id, EventKind::AgentLog { line: line.into() })?;
        Ok(())
    }

    /// Moves the run to `next` (compare-and-set against the last known status).
    fn set(&mut self, next: RunStatus) -> Result<()> {
        self.inner
            .store
            .transition(self.id, &self.status, next.clone())?;
        match &next {
            RunStatus::Provisioning | RunStatus::Running => self.clock.run(),
            // The agent waits inside a tool call, and its sandbox keeps working meanwhile.
            RunStatus::Suspended(SuspendReason::Approval(id)) if self.asking == Some(*id) => {
                self.clock.run();
            }
            _ => self.clock.pause(),
        }
        self.status = next;
        if matches!(self.status, RunStatus::Suspended(_)) {
            self.sync_slot();
        }
        Ok(())
    }
}

/// The error for a `create` task that panicked or was cancelled.
fn create_died(e: &tokio::task::JoinError) -> kestrel_sandbox::SandboxError {
    kestrel_sandbox::SandboxError::Spawn {
        program: "sandbox create".into(),
        source: std::io::Error::other(e.to_string()),
    }
}

/// Wall clock budget that only counts while the run is provisioning or running.
struct WallClock {
    left: Duration,
    since: Option<Instant>,
}

impl WallClock {
    fn new(limit: Duration) -> WallClock {
        WallClock {
            left: limit,
            since: None,
        }
    }

    fn run(&mut self) {
        self.since.get_or_insert_with(Instant::now);
    }

    fn pause(&mut self) {
        if let Some(since) = self.since.take() {
            self.left = self.left.saturating_sub(since.elapsed());
        }
    }

    /// When the limit is reached, if the clock is running (and the instant is representable).
    fn deadline(&self) -> Option<Instant> {
        self.since.and_then(|s| s.checked_add(self.left))
    }
}

/// Resolves at `deadline`, or never.
async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// The next command. The engine drops the sender only when it shuts down, and then aborts this
/// task, so a closed channel just waits.
async fn recv(cmds: &mut mpsc::UnboundedReceiver<ControlCommand>) -> ControlCommand {
    match cmds.recv().await {
        Some(cmd) => cmd,
        None => std::future::pending().await,
    }
}
