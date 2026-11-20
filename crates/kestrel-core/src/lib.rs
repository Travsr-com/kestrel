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
mod slots;
pub mod workspace;
pub use workspace::{WorkspaceError, Workspaces};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use kestrel_agent::{ParsedLine, RunContext, RuntimeAdapter};
use kestrel_sandbox::{
    INSTANCE_LABEL, LaunchSpec, NetworkPolicy, RUN_LABEL, SandboxDriver, SandboxHandle,
};
use kestrel_store::{ControlCommand, RunFilter, Store, StoreError};
use kestrel_types::{
    AgentSpec, ApprovalId, EventKind, MicroUsd, RunId, RunStatus, Runtime, SandboxTier,
    SuspendReason, Tier, WorkspaceSpec,
};
use tokio::sync::{Notify, mpsc};
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
    /// `recover` was called while `run_forever` is active.
    #[error("the engine is already running; wait until run_forever has returned")]
    AlreadyRunning,
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Builds a fresh runtime adapter. The engine calls it once per run and never shares an adapter
/// across runs. In production this is `kestrel_agent::adapter_for`.
pub type AdapterFactory = Arc<dyn Fn(Runtime) -> Box<dyn RuntimeAdapter> + Send + Sync>;

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
/// is not dropped. `SetBudget` and `Cancel` still apply at once, and a `Cancel` removes the
/// pending resume. A resume still pending when the engine stops is lost and must be sent again
/// (the run stays suspended). After [`Engine::recover`] no run holds a slot, and commands for the
/// recovered runs wait for one like any other.
///
/// Everything waiting for a slot, `Queued` runs and pending resumes alike, is served in one FIFO
/// order. Each waiter gets an arrival number. Within one scheduler tick, newly seen `Queued` runs
/// are numbered first (in creation order), then newly claimed resume commands (in control id
/// order). Slots go to the lowest arrival number first.
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
pub struct Engine {
    inner: Arc<Inner>,
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
    /// What lives outside the run actors, which are aborted when `run_forever` returns.
    live: Mutex<Live>,
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
}

/// A queued run, or a resume command that needs a slot.
struct Waiter {
    run: RunId,
    /// `None` for a `Queued` run to start.
    resume: Option<ControlCommand>,
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
                live: Mutex::default(),
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
        let mut tick = self.inner.ticker();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = stop.cancelled() => return Ok(()),
                _ = tick.tick() => self.inner.poll(&mut scheduler)?,
            }
        }
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
                Ok(()) => report.destroyed.push(run),
                Err(e) => report.failed.push((run, e.to_string())),
            }
        }
        report.timed_out.extend(left);
        report
    }

    /// Call once at startup, before `run_forever`.
    ///
    /// `Running` runs become `Suspended(Error("engine restarted"))`, `Provisioning` runs fail,
    /// `Suspended` runs stay suspended and `Queued` runs stay queued. Sandboxes left behind by the
    /// previous engine are destroyed; a resumed run gets a fresh one.
    ///
    /// Refused with [`EngineError::AlreadyRunning`] while `run_forever` is active.
    pub async fn recover(&self) -> Result<RecoveryReport> {
        if self.inner.live().active.is_some() {
            return Err(EngineError::AlreadyRunning);
        }
        let store = &self.inner.store;
        self.inner.sweep_leftovers().await;
        let mut report = RecoveryReport::default();
        for run in store.list_runs(RunFilter::default())? {
            if run.status.is_terminal() || run.status == RunStatus::Queued {
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
                _ => report.suspended.push(run.id),
            }
        }
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
        Ok(Active(self))
    }

    /// Every slot free again, as the actors that held them are gone.
    fn reset_slots(&self) {
        *self.slots() = Slots::new(self.limits.max_concurrent_runs);
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
        self.line_up_queued(s)?;
        s.waiting.extend(resumes);
        self.grant(s)
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
        }
        let needs_slot = matches!(cmd, ControlCommand::Resume | ControlCommand::Approve(_));
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
            .filter(|w| w.resume.is_none())
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
            });
        }
        Ok(())
    }

    /// Gives free slots to the waiters, first come first served. When the head of the line finds
    /// no slot, nobody behind it can: every waiter needs exactly one.
    fn grant(self: &Arc<Self>, s: &mut Scheduler) -> Result<()> {
        while let Some(waiter) = s.waiting.pop_front() {
            let got = {
                let mut slots = self.slots();
                match waiter.resume {
                    Some(_) => slots.reacquire(waiter.run),
                    None => slots.try_acquire(waiter.run),
                }
            };
            if !got {
                s.waiting.push_front(waiter);
                break;
            }
            let started = match &waiter.resume {
                None => self.start_queued(waiter.run, s)?,
                Some(cmd) => self.start_resume(waiter.run, cmd.clone(), s)?,
            };
            if !started {
                self.slots().release(waiter.run);
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
                self.store.set_budget(run, limit)?;
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

    /// The per-run gateway URL, `{gateway}/r/{token}`.
    fn run_gateway(&self, token: &str) -> Url {
        let mut url = self.gateway_url.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().push("r").push(token);
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

    /// Best effort: destroys the last sandbox a previous engine provisioned for `run`.
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
        Ok(())
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

/// An `AgentLog` noting that `cmd` does not apply to a run in `status`.
fn ignored(cmd: &ControlCommand, status: &RunStatus) -> EventKind {
    EventKind::AgentLog {
        line: format!("kestrel: ignored {cmd:?} while the run is {status:?}"),
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
    warned: bool,
    clock: WallClock,
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
            warned: false,
            clock,
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
        if let Some(sb) = self.sandbox.take() {
            let _ = self.inner.destroy_tracked(sb.driver, &sb.handle).await;
        }
        // The run ended; whatever slot it still holds goes back.
        self.inner.slots().release(self.id);
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
        let next = self.on_command(cmd)?;
        // An approval granted after a restart leaves the run `Running` with nothing attached.
        let go = matches!(next, Next::Resume) || self.status == RunStatus::Running;
        if !go {
            // A resume that was declined, or a cancel: nothing here needs the slot taken for it.
            self.sync_slot();
        }
        Ok(go)
    }

    /// Creates the sandbox, applying commands while `create` runs. `None` when the run ended
    /// meanwhile (the sandbox, if created, is destroyed on exit).
    async fn provision(&mut self) -> Result<Option<Sandbox>> {
        let inner = self.inner.clone();
        let idx = inner
            .driver_for(self.spec.sandbox.tier)
            .ok_or(EngineError::NoDriver)?;
        let driver = &inner.drivers[idx];
        let mut launch = self.launch_spec()?;
        launch.workspace = self.workspace().await?;
        let mut create = inner.start_create(self.id, idx, launch);
        let created = loop {
            let deadline = self.clock.deadline();
            tokio::select! {
                res = &mut create => break res.unwrap_or_else(|e| Err(create_died(&e))),
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
        Ok(Some(sb))
    }

    /// Prepares this run's workspace, if the engine has workspaces. Reuses an existing one, so
    /// a resumed run gets the same directory back.
    async fn workspace(&self) -> Result<Option<PathBuf>> {
        let Some(workspaces) = &self.inner.workspaces else {
            return Ok(None);
        };
        let spec = &self.spec.sandbox.workspace;
        if let WorkspaceSpec::GitClone { repo, .. } = spec
            && !repo.is_absolute()
        {
            return Err(EngineError::RelativeRepo { repo: repo.clone() });
        }
        Ok(Some(workspaces.prepare(self.id, spec).await?))
    }

    fn launch_spec(&self) -> Result<LaunchSpec> {
        let ctx = RunContext {
            run_id: self.id,
            gateway_url: self.inner.run_gateway(&self.token),
            model: self.inner.model(self.spec.model_policy.default),
        };
        let plan = self.adapter.launch_plan(&self.spec, &self.input, &ctx)?;
        let mut labels = BTreeMap::from([(RUN_LABEL.to_string(), self.id.to_string())]);
        if let Some(instance) = &self.inner.instance {
            labels.insert(INSTANCE_LABEL.to_string(), instance.clone());
        }
        Ok(LaunchSpec {
            image: plan.image,
            argv: plan.argv,
            env: plan.env,
            workspace: None,
            cpu_millis: self.spec.sandbox.cpu_millis,
            memory_mb: self.spec.sandbox.memory_mb,
            network: NetworkPolicy::GatewayOnly {
                gateway: self.inner.gateway_url.clone(),
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
                self.inner.store.set_budget(self.id, limit)?;
                self.spec.budget.limit = limit;
                // One warning per limit.
                self.warned = false;
                Ok(Next::Continue)
            }
            C::Pause if self.status == RunStatus::Running => {
                self.set(RunStatus::Suspended(SuspendReason::Manual))?;
                Ok(Next::Stop)
            }
            C::Resume if matches!(self.status, RunStatus::Suspended(_)) && pending.is_none() => {
                let over = self.status == RunStatus::Suspended(SuspendReason::Budget)
                    && self.spent()? >= self.spec.budget.limit;
                if over {
                    self.log("kestrel: not resumed, the run is still over its budget")?;
                    return Ok(Next::Continue);
                }
                if !self.ensure_slot()? {
                    return Ok(Next::Continue);
                }
                Ok(Next::Resume)
            }
            C::Approve(id) | C::Reject(id) if pending != Some(id) => {
                self.log(&format!(
                    "kestrel: no pending approval {id}, command ignored"
                ))?;
                Ok(Next::Continue)
            }
            C::Approve(_) if !self.ensure_slot()? => Ok(Next::Continue),
            C::Approve(id) => {
                self.resolve(id, true)?;
                self.set(RunStatus::Running)?;
                Ok(Next::Continue)
            }
            C::Reject(id) => {
                self.resolve(id, false)?;
                self.set(RunStatus::Failed(format!("approval {id} rejected")))?;
                Ok(Next::Stop)
            }
            other => {
                self.inner
                    .store
                    .append(self.id, ignored(&other, &self.status))?;
                Ok(Next::Continue)
            }
        }
    }

    fn resolve(&self, id: ApprovalId, approved: bool) -> Result<()> {
        self.inner
            .store
            .append(self.id, EventKind::ApprovalResolved { id, approved })?;
        Ok(())
    }

    /// While `Running`: one `BudgetWarning` at 80% of the limit, `Suspended(Budget)` at 100%.
    fn check_budget(&mut self) -> Result<Next> {
        if self.status != RunStatus::Running {
            return Ok(Next::Continue);
        }
        let spent = self.spent()?;
        let limit = self.spec.budget.limit;
        if spent >= limit {
            self.set(RunStatus::Suspended(SuspendReason::Budget))?;
            return Ok(Next::Stop);
        }
        if !self.warned && i128::from(spent.0) * 5 >= i128::from(limit.0) * 4 {
            self.warned = true;
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
        match next {
            RunStatus::Provisioning | RunStatus::Running => self.clock.run(),
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
