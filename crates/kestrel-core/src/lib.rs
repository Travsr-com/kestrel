//! Scheduler, run state machine, budgets and approvals.
//!
//! The [`Engine`] drives a run from `Queued` to a terminal status: it provisions a sandbox, launches
//! the agent through its runtime adapter, streams the agent's events into the [`Store`], enforces
//! budgets and the wall clock, and applies control commands (pause, resume, cancel, approve, reject,
//! set budget) read from [`Store::take_control`].
//!
//! Each active run has its own task (a run actor). It is the only writer of that run's status, so
//! control commands are applied while the run is busy, for example while `create` is still blocked.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kestrel_agent::{ParsedLine, RunContext, RuntimeAdapter};
use kestrel_sandbox::{LaunchSpec, NetworkPolicy, SandboxDriver, SandboxHandle};
use kestrel_store::{ControlCommand, RunFilter, Store, StoreError};
use kestrel_types::{
    AgentSpec, ApprovalId, EventKind, MicroUsd, RunId, RunStatus, Runtime, SandboxTier,
    SuspendReason, Tier,
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Instant, Interval, MissedTickBehavior};
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

/// Status message of a run that was `Running` when the previous engine stopped.
const ENGINE_RESTARTED: &str = "engine restarted";
/// Failure message of a run past its wall clock limit.
const WALL_CLOCK_EXCEEDED: &str = "wall clock exceeded";

/// The run engine.
///
/// Known M1 limitations:
///
/// * Resuming a paused or budget-suspended run starts the stopped sandbox again, which restarts
///   the agent command from the beginning. The container survives, the agent's process does not.
/// * A run resumed by a new engine after a restart gets a fresh sandbox, and its wall clock starts
///   over.
/// * A run holds its concurrency slot while it is suspended (its sandbox still exists). A command
///   for a suspended run that no actor holds (after a restart) starts one even past the cap.
/// * The wall clock counts only time spent `Provisioning` or `Running`, not time suspended.
/// * Workspaces are not prepared yet: the sandbox gets no `/workspace` mount.
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
}

/// Control channels of the run actors, by run.
type Actors = HashMap<RunId, mpsc::UnboundedSender<ControlCommand>>;

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
                limits,
                models,
            }),
        }
    }

    /// Creates a `Queued` run and writes its `RunCreated` event. The spec is persisted so a later
    /// engine on the same database can resume the run.
    pub async fn submit(&self, spec: AgentSpec, input: String) -> Result<RunId> {
        let store = &self.inner.store;
        let id = store.create_run(&spec, &input)?;
        store.append(id, EventKind::RunCreated { agent: spec.name })?;
        Ok(id)
    }

    /// Schedules queued runs, applies control commands and enforces limits until `shutdown`.
    ///
    /// On shutdown the run actors are dropped without touching their runs; [`Engine::recover`]
    /// picks them up on the next start.
    pub async fn run_forever(&self, shutdown: CancellationToken) -> Result<()> {
        let mut actors = Actors::new();
        // Dropping the set on return aborts every run actor.
        let mut tasks = JoinSet::new();
        let mut tick = self.inner.ticker();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = tick.tick() => self.inner.poll(&mut actors, &mut tasks)?,
            }
        }
    }

    /// Call once at startup, before `run_forever`.
    ///
    /// `Running` runs become `Suspended(Error("engine restarted"))`, `Provisioning` runs fail,
    /// `Suspended` runs stay suspended and `Queued` runs stay queued. Sandboxes left behind by the
    /// previous engine are destroyed; a resumed run gets a fresh one.
    pub async fn recover(&self) -> Result<RecoveryReport> {
        let store = &self.inner.store;
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
    /// One scheduler tick: forget finished actors, route control commands, start queued runs.
    fn poll(self: &Arc<Self>, actors: &mut Actors, tasks: &mut JoinSet<()>) -> Result<()> {
        while tasks.try_join_next().is_some() {}
        actors.retain(|_, tx| !tx.is_closed());

        for (_, run, cmd) in self.store.take_control()? {
            let cmd = match actors.get(&run) {
                Some(tx) => match tx.send(cmd) {
                    Ok(()) => continue,
                    // The actor finished since the check above.
                    Err(mpsc::error::SendError(cmd)) => cmd,
                },
                None => cmd,
            };
            self.idle_command(run, cmd, actors, tasks)?;
        }

        let free = self.limits.max_concurrent_runs.saturating_sub(actors.len());
        if free == 0 {
            return Ok(());
        }
        let queued = self.store.list_runs(RunFilter {
            status: Some(RunStatus::Queued),
            limit: Some(free),
            ..RunFilter::default()
        })?;
        for run in queued {
            // A conflict means the run was cancelled meanwhile; it is not ours to start.
            let claimed =
                self.store
                    .transition(run.id, &RunStatus::Queued, RunStatus::Provisioning);
            if claimed.is_ok() {
                self.spawn(run.id, RunStatus::Provisioning, run.input, actors, tasks)?;
            }
        }
        Ok(())
    }

    /// A command for a run that has no actor in this engine.
    fn idle_command(
        self: &Arc<Self>,
        run: RunId,
        cmd: ControlCommand,
        actors: &mut Actors,
        tasks: &mut JoinSet<()>,
    ) -> Result<()> {
        let Some(row) = self.store.get_run(run)? else {
            return Ok(());
        };
        match (&row.status, cmd) {
            // Suspended before a restart: an actor takes it over from here.
            (RunStatus::Suspended(_), cmd) => {
                self.spawn(run, row.status.clone(), row.input, actors, tasks)?;
                if let Some(tx) = actors.get(&run) {
                    // The actor was just created and holds its receiver.
                    let _ = tx.send(cmd);
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
        actors: &mut Actors,
        tasks: &mut JoinSet<()>,
    ) -> Result<()> {
        let (Some(spec), Some(token)) = (self.store.run_spec(run)?, self.store.run_token(run)?)
        else {
            let failed =
                RunStatus::Failed("no agent spec or run token is stored for this run".into());
            self.store.transition(run, &status, failed)?;
            return Ok(());
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Actor::new(self.clone(), run, spec, input, token, status, rx);
        tasks.spawn(actor.run());
        actors.insert(run, tx);
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
            && let Some(driver) = self.drivers.iter().find(|d| d.name() == name)
        {
            // The sandbox may well be gone already.
            let _ = driver.destroy(&SandboxHandle { id }).await;
        }
        Ok(())
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
            let _ = self.set(RunStatus::Failed(e.to_string()));
        }
        if let Some(sb) = self.sandbox.take() {
            let _ = self.inner.drivers[sb.driver].destroy(&sb.handle).await;
        }
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
        Ok(matches!(next, Next::Resume) || self.status == RunStatus::Running)
    }

    /// Creates the sandbox, applying commands while `create` runs. `None` when the run ended
    /// meanwhile (the sandbox, if created, is destroyed on exit).
    async fn provision(&mut self) -> Result<Option<Sandbox>> {
        let inner = self.inner.clone();
        let idx = inner
            .driver_for(self.spec.sandbox.tier)
            .ok_or(EngineError::NoDriver)?;
        let driver = &inner.drivers[idx];
        let launch = self.launch_spec()?;
        let mut create = driver.create(&launch);
        let created = loop {
            let deadline = self.clock.deadline();
            tokio::select! {
                res = &mut create => break res,
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

    fn launch_spec(&self) -> Result<LaunchSpec> {
        let ctx = RunContext {
            run_id: self.id,
            gateway_url: self.inner.run_gateway(&self.token),
            model: self.inner.model(self.spec.model_policy.default),
        };
        let plan = self.adapter.launch_plan(&self.spec, &self.input, &ctx)?;
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

    fn on_line(&mut self, line: &str, finished: &mut Finished) -> Result<Next> {
        match self.adapter.parse_line(line) {
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
                Ok(Next::Resume)
            }
            C::Approve(id) | C::Reject(id) if pending != Some(id) => {
                self.log(&format!(
                    "kestrel: no pending approval {id}, command ignored"
                ))?;
                Ok(Next::Continue)
            }
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
        Ok(())
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
