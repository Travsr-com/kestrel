//! Fakes for the engine tests: a scripted sandbox driver, a scripted runtime adapter, a temp-file
//! store and helpers that let the engine work under a paused clock. Nothing here sleeps.

// Helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the right
// way to fail here.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use kestrel_agent::{LaunchPlan, ParsedLine, RunContext, RuntimeAdapter};
use kestrel_core::{AdapterFactory, Engine, Limits};
use kestrel_sandbox::{
    ExitStatus, LaunchSpec, OutputLine, OutputStream, SandboxDriver, SandboxError, SandboxHandle,
    StdStream,
};
use kestrel_store::{ControlCommand, Store};
use kestrel_types::{
    AgentSpec, ApprovalId, EventKind, MicroUsd, ModelCallRecord, RunId, RunStatus, Runtime,
};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

pub const GATEWAY: &str = "http://127.0.0.1:7070/";

/// Poll interval the engine is configured with in every test.
pub const POLL: Duration = Duration::from_millis(50);

/// Exit code the fake reports for a sandbox that was stopped.
pub const STOPPED_EXIT: i32 = 137;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Agent spec and model calls

/// An agent spec with the given budget (dollars) and wall clock (for example `30m`).
pub fn spec_with(usd: &str, wall_clock: &str) -> AgentSpec {
    let yaml = format!(
        "name: tester\nruntime: claude-code\nrole: tester\n\
         model_policy: {{ default: tier:mid }}\n\
         sandbox: {{ tier: auto, image: fake-image, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
         budget: {{ usd: \"{usd}\", wall_clock: {wall_clock} }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture spec parses")
}

/// $1 budget, one hour of wall clock: neither limit is reached unless a test means to.
pub fn spec() -> AgentSpec {
    spec_with("1", "1h")
}

/// A model call as the gateway would record it.
pub fn model_call(run: RunId, cost: i64) -> ModelCallRecord {
    ModelCallRecord {
        run_id: run,
        provider: "anthropic".into(),
        model: "test-model".into(),
        tokens_in: 100,
        tokens_out: 50,
        tokens_cache_read: 0,
        tokens_cache_write: 0,
        cost: Some(MicroUsd(cost)),
        latency_ms: 10,
        status: 200,
    }
}

// ---------------------------------------------------------------------------------------------
// FakeDriver

/// Every call the engine made on the fake driver, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Create,
    Start(String),
    Wait(String),
    Stop(String),
    Destroy(String),
}

/// Test side of one `start`: feed output lines, then end the process with an exit code.
pub struct SessionCtl {
    lines: Option<mpsc::UnboundedSender<String>>,
    exit: watch::Sender<Option<i32>>,
}

impl SessionCtl {
    /// The agent prints `line` on stdout.
    pub fn line(&self, line: &str) {
        if let Some(tx) = &self.lines {
            // The engine may have stopped the sandbox already; then the line goes nowhere.
            let _ = tx.send(line.to_string());
        }
    }

    /// The agent closes its output and exits with `code`.
    pub fn exit(mut self, code: i32) {
        self.lines.take();
        self.exit.send_replace(Some(code));
    }
}

/// Driver side of one `start`.
struct Session {
    lines: mpsc::UnboundedReceiver<String>,
    exit: watch::Receiver<Option<i32>>,
}

fn session() -> (SessionCtl, Session) {
    let (ltx, lrx) = mpsc::unbounded_channel();
    let (etx, erx) = watch::channel(None);
    (
        SessionCtl {
            lines: Some(ltx),
            exit: etx,
        },
        Session {
            lines: lrx,
            exit: erx,
        },
    )
}

/// The current process of a started sandbox.
#[derive(Clone)]
struct Active {
    stop: CancellationToken,
    drained: watch::Receiver<bool>,
    exit: watch::Receiver<Option<i32>>,
}

#[derive(Default)]
struct DriverState {
    calls: Mutex<Vec<Call>>,
    specs: Mutex<Vec<LaunchSpec>>,
    /// Consumed one per `create`, before any success.
    create_failures: Mutex<VecDeque<String>>,
    /// When set, `create` waits for one `notify_one` before returning.
    create_gate: Mutex<Option<Arc<Notify>>>,
    /// Consumed one per `start`, in order.
    sessions: Mutex<VecDeque<Session>>,
    /// Controllers of sessions made up for an unscripted `start`; kept so their output stays open.
    orphans: Mutex<Vec<SessionCtl>>,
    active: Mutex<HashMap<String, Active>>,
    next_id: Mutex<usize>,
}

/// A sandbox driver that runs nothing. Output and exit codes come from sessions the test queues.
///
/// Like a real pipe, `wait` only resolves once the output stream from `start` has been read to the
/// end, so an engine that stops reading output before waiting hangs (and the test fails). Dropping
/// the stream early makes `wait` report -1, as the Apple driver does. `stop` and `destroy` end the
/// current output and make `wait` report [`STOPPED_EXIT`].
#[derive(Clone, Default)]
pub struct FakeDriver(Arc<DriverState>);

impl FakeDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a session whose output is `lines` followed by exit `code`.
    pub fn script(&self, lines: &[&str], code: i32) {
        let ctl = self.live();
        for l in lines {
            ctl.line(l);
        }
        ctl.exit(code);
    }

    /// Queues a session the test drives line by line.
    pub fn live(&self) -> SessionCtl {
        let (ctl, s) = session();
        lock(&self.0.sessions).push_back(s);
        ctl
    }

    /// The next `create` fails with `stderr` as the CLI's error text.
    pub fn fail_next_create(&self, stderr: &str) {
        lock(&self.0.create_failures).push_back(stderr.to_string());
    }

    /// Every later `create` blocks until the returned `Notify` is notified once.
    pub fn gate_create(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *lock(&self.0.create_gate) = Some(gate.clone());
        gate
    }

    pub fn calls(&self) -> Vec<Call> {
        lock(&self.0.calls).clone()
    }

    pub fn specs(&self) -> Vec<LaunchSpec> {
        lock(&self.0.specs).clone()
    }

    pub fn count(&self, call: &Call) -> usize {
        self.calls().iter().filter(|c| *c == call).count()
    }

    pub fn starts(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, Call::Start(_)))
            .count()
    }

    /// Sandboxes created and never destroyed.
    pub fn leaked(&self) -> Vec<String> {
        let calls = self.calls();
        let created: Vec<String> = (1..=*lock(&self.0.next_id))
            .map(|n| format!("fake-{n}"))
            .collect();
        created
            .into_iter()
            .filter(|id| !calls.contains(&Call::Destroy(id.clone())))
            .collect()
    }

    fn record(&self, call: Call) {
        lock(&self.0.calls).push(call);
    }

    fn active(&self, id: &str) -> Option<Active> {
        lock(&self.0.active).get(id).cloned()
    }
}

#[async_trait]
impl SandboxDriver for FakeDriver {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn is_strong(&self) -> bool {
        true
    }

    async fn create(&self, spec: &LaunchSpec) -> kestrel_sandbox::Result<SandboxHandle> {
        self.record(Call::Create);
        lock(&self.0.specs).push(spec.clone());
        let gate = lock(&self.0.create_gate).clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if let Some(stderr) = lock(&self.0.create_failures).pop_front() {
            return Err(SandboxError::CommandFailed {
                program: "fake".into(),
                code: 125,
                stderr,
            });
        }
        let mut n = lock(&self.0.next_id);
        *n += 1;
        Ok(SandboxHandle {
            id: format!("fake-{n}"),
        })
    }

    async fn start(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<OutputStream> {
        self.record(Call::Start(h.id.clone()));
        let next = lock(&self.0.sessions).pop_front();
        let s = match next {
            Some(s) => s,
            None => {
                let (ctl, s) = session();
                lock(&self.0.orphans).push(ctl);
                s
            }
        };
        let stop = CancellationToken::new();
        let (drained_tx, drained_rx) = watch::channel(false);
        lock(&self.0.active).insert(
            h.id.clone(),
            Active {
                stop: stop.clone(),
                drained: drained_rx,
                exit: s.exit,
            },
        );
        let stream = futures::stream::unfold(
            (s.lines, stop, Some(drained_tx)),
            |(mut rx, stop, drained)| async move {
                let drained = drained?;
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => {
                        drained.send_replace(true);
                        None
                    }
                    line = rx.recv() => match line {
                        Some(line) => Some((
                            OutputLine { stream: StdStream::Stdout, line },
                            (rx, stop, Some(drained)),
                        )),
                        None => {
                            drained.send_replace(true);
                            None
                        }
                    },
                }
            },
        );
        Ok(Box::pin(stream))
    }

    async fn wait(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<ExitStatus> {
        self.record(Call::Wait(h.id.clone()));
        let Some(mut a) = self.active(&h.id) else {
            return Err(SandboxError::NotAttached { id: h.id.clone() });
        };
        let exited = async {
            let drained = a.drained.wait_for(|d| *d).await.is_ok();
            if !drained {
                // The output stream was dropped before its end.
                return -1;
            }
            let code = a.exit.wait_for(|e| e.is_some()).await.map(|c| *c);
            match code {
                Ok(code) => code.unwrap_or(-1),
                Err(_) => std::future::pending().await,
            }
        };
        let code = tokio::select! {
            _ = a.stop.cancelled() => STOPPED_EXIT,
            code = exited => code,
        };
        Ok(ExitStatus { code })
    }

    async fn stop(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.record(Call::Stop(h.id.clone()));
        if let Some(a) = self.active(&h.id) {
            a.stop.cancel();
        }
        Ok(())
    }

    async fn destroy(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.record(Call::Destroy(h.id.clone()));
        if let Some(a) = lock(&self.0.active).remove(&h.id) {
            a.stop.cancel();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// FakeAdapter

/// What every adapter instance saw, by instance number (creation order).
#[derive(Default)]
pub struct AdapterLog {
    runtimes: Mutex<Vec<Runtime>>,
    lines: Mutex<Vec<Vec<String>>>,
    plans: Mutex<Vec<Vec<RunContext>>>,
    files: Mutex<Vec<kestrel_agent::PlanFile>>,
}

impl AdapterLog {
    /// The files every plan from now on carries.
    pub fn set_files(&self, files: Vec<kestrel_agent::PlanFile>) {
        *lock(&self.files) = files;
    }

    pub fn instances(&self) -> usize {
        lock(&self.runtimes).len()
    }

    pub fn runtimes(&self) -> Vec<Runtime> {
        lock(&self.runtimes).clone()
    }

    /// Lines passed to `parse_line` of each instance.
    pub fn lines(&self) -> Vec<Vec<String>> {
        lock(&self.lines).clone()
    }

    /// Contexts passed to `launch_plan` of each instance.
    pub fn plans(&self) -> Vec<Vec<RunContext>> {
        lock(&self.plans).clone()
    }

    fn new_instance(&self, rt: Runtime) -> usize {
        let mut rts = lock(&self.runtimes);
        rts.push(rt);
        lock(&self.lines).push(Vec::new());
        lock(&self.plans).push(Vec::new());
        rts.len() - 1
    }
}

/// Parses a tiny line language:
///
/// * `msg:TEXT` gives `AgentMessage { text }`
/// * `ok:SUMMARY` / `fail:SUMMARY` give `Finished { ok, summary }`
/// * `approval:UUID:ACTION` gives `ApprovalRequested { id, action, question: false }`
/// * an empty line is ignored, anything else is an `AgentLog`
pub struct FakeAdapter {
    instance: usize,
    log: Arc<AdapterLog>,
}

impl RuntimeAdapter for FakeAdapter {
    fn runtime(&self) -> Runtime {
        Runtime::ClaudeCode
    }

    fn launch_plan(
        &self,
        spec: &AgentSpec,
        input: &str,
        ctx: &RunContext,
    ) -> kestrel_agent::Result<LaunchPlan> {
        if let Some(p) = lock(&self.log.plans).get_mut(self.instance) {
            p.push(ctx.clone());
        }
        Ok(LaunchPlan {
            image: spec
                .sandbox
                .image
                .clone()
                .unwrap_or_else(|| "fake-image".into()),
            argv: vec!["fake-agent".into(), input.into()],
            env: BTreeMap::new(),
            files: lock(&self.log.files).clone(),
        })
    }

    fn parse_line(&self, line: &str) -> ParsedLine {
        if let Some(l) = lock(&self.log.lines).get_mut(self.instance) {
            l.push(line.to_string());
        }
        parse(line)
    }
}

fn parse(line: &str) -> ParsedLine {
    if line.is_empty() {
        return ParsedLine::Ignore;
    }
    if let Some(text) = line.strip_prefix("msg:") {
        return ParsedLine::Events(vec![EventKind::AgentMessage { text: text.into() }]);
    }
    if let Some(s) = line.strip_prefix("ok:") {
        return ParsedLine::Finished {
            ok: true,
            summary: Some(s.into()),
        };
    }
    if let Some(s) = line.strip_prefix("fail:") {
        return ParsedLine::Finished {
            ok: false,
            summary: Some(s.into()),
        };
    }
    if let Some(rest) = line.strip_prefix("approval:")
        && let Some((id, action)) = rest.split_once(':')
        && let Ok(id) = id.parse::<ApprovalId>()
    {
        return ParsedLine::Events(vec![EventKind::ApprovalRequested {
            id,
            action: action.into(),
            question: false,
        }]);
    }
    ParsedLine::Events(vec![EventKind::AgentLog { line: line.into() }])
}

/// A factory that hands out a new [`FakeAdapter`] per call and logs into `log`.
pub fn adapter_factory(log: Arc<AdapterLog>) -> AdapterFactory {
    Arc::new(move |rt| {
        let instance = log.new_instance(rt);
        Box::new(FakeAdapter {
            instance,
            log: log.clone(),
        }) as Box<dyn RuntimeAdapter>
    })
}

// ---------------------------------------------------------------------------------------------
// Harness

pub struct Harness {
    pub store: Arc<Store>,
    pub engine: Arc<Engine>,
    pub driver: FakeDriver,
    pub adapters: Arc<AdapterLog>,
    pub shutdown: CancellationToken,
    pub task: Option<JoinHandle<kestrel_core::Result<()>>>,
    pub db: PathBuf,
    /// `max_concurrent_runs` of the engine, kept for [`Harness::rebuild`].
    pub max: usize,
    _dir: Option<TempDir>,
}

impl Harness {
    /// An engine on a fresh temp-file store, with `max_concurrent_runs = max`.
    pub fn new(max: usize) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("kestrel.db");
        let mut h = Self::on(&db, max);
        h._dir = Some(dir);
        h
    }

    /// An engine on the database at `db` (which the caller keeps alive).
    pub fn on(db: &Path, max: usize) -> Self {
        let store = Arc::new(Store::open(db).expect("open store"));
        let driver = FakeDriver::new();
        let adapters = Arc::new(AdapterLog::default());
        let engine = Engine::new(
            store.clone(),
            vec![Box::new(driver.clone())],
            adapter_factory(adapters.clone()),
            Url::parse(GATEWAY).expect("gateway url"),
            Limits {
                max_concurrent_runs: max,
                poll_interval: POLL,
            },
        );
        Self {
            store,
            engine: Arc::new(engine),
            driver,
            adapters,
            shutdown: CancellationToken::new(),
            task: None,
            db: db.to_path_buf(),
            max,
            _dir: None,
        }
    }

    /// Replaces the engine with a fresh one on the same store, driver and adapters, passed through
    /// `f` (for example to add `with_root_key`). Call before [`Harness::start`].
    pub fn rebuild(&mut self, f: impl FnOnce(Engine) -> Engine) {
        let engine = Engine::new(
            self.store.clone(),
            vec![Box::new(self.driver.clone())],
            adapter_factory(self.adapters.clone()),
            Url::parse(GATEWAY).expect("gateway url"),
            Limits {
                max_concurrent_runs: self.max,
                poll_interval: POLL,
            },
        );
        self.engine = Arc::new(f(engine));
    }

    /// Spawns `run_forever`.
    pub fn start(&mut self) {
        let engine = self.engine.clone();
        let token = self.shutdown.clone();
        self.task = Some(tokio::spawn(async move { engine.run_forever(token).await }));
    }

    pub async fn submit(&self, spec: AgentSpec) -> RunId {
        self.engine
            .submit(spec, "do the task".into())
            .await
            .expect("submit")
    }

    pub fn status(&self, id: RunId) -> RunStatus {
        self.store
            .get_run(id)
            .expect("get_run")
            .expect("run exists")
            .status
    }

    pub fn control(&self, id: RunId, cmd: ControlCommand) {
        self.store.push_control(id, cmd).expect("push_control");
    }

    pub fn events(&self, id: RunId) -> Vec<EventKind> {
        self.store
            .events_since(id, 0)
            .expect("events_since")
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    /// Every event except `StatusChanged`, in order.
    pub fn non_status_events(&self, id: RunId) -> Vec<EventKind> {
        self.events(id)
            .into_iter()
            .filter(|k| !matches!(k, EventKind::StatusChanged { .. }))
            .collect()
    }

    /// The `(from, to)` pairs of every `StatusChanged` event, in order.
    pub fn transitions(&self, id: RunId) -> Vec<(RunStatus, RunStatus)> {
        self.events(id)
            .into_iter()
            .filter_map(|k| match k {
                EventKind::StatusChanged { from, to } => Some((from, to)),
                _ => None,
            })
            .collect()
    }

    pub fn has_event(&self, id: RunId, pred: impl Fn(&EventKind) -> bool) -> bool {
        self.events(id).iter().any(pred)
    }

    pub fn count_events(&self, id: RunId, pred: impl Fn(&EventKind) -> bool) -> usize {
        self.events(id).iter().filter(|k| pred(k)).count()
    }

    /// The sandbox id from the run's `SandboxProvisioned` event.
    pub fn sandbox_id(&self, id: RunId) -> Option<String> {
        self.events(id).into_iter().find_map(|k| match k {
            EventKind::SandboxProvisioned { sandbox_id, .. } => Some(sandbox_id),
            _ => None,
        })
    }

    pub fn is(&self, id: RunId, status: &RunStatus) -> bool {
        &self.status(id) == status
    }

    pub fn is_failed(&self, id: RunId) -> Option<String> {
        match self.status(id) {
            RunStatus::Failed(m) => Some(m),
            _ => None,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

// ---------------------------------------------------------------------------------------------
// Letting the engine work under a paused clock

/// Most virtual time one `until` call may advance, so tests far below their wall clock limit never
/// reach it by accident.
const MAX_ADVANCE: Duration = Duration::from_secs(20);
const STEP: Duration = Duration::from_millis(10);
/// Real-time bound before a wait gives up. The loop never sleeps; it yields and advances the paused
/// clock, so a passing test finishes in milliseconds.
const REAL_DEADLINE: Duration = Duration::from_secs(10);

async fn yield_a_bit() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// Lets the engine run, advancing the paused clock in small steps, until `cond` holds. Panics
/// naming `what` if it never does.
pub async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + REAL_DEADLINE;
    let mut advanced = Duration::ZERO;
    loop {
        if cond() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        yield_a_bit().await;
        if advanced < MAX_ADVANCE {
            tokio::time::advance(STEP).await;
            advanced += STEP;
        }
    }
}

/// Lets the engine run for `ticks` poll intervals of virtual time.
pub async fn settle_for(ticks: u32) {
    for _ in 0..ticks * 5 {
        yield_a_bit().await;
        tokio::time::advance(STEP).await;
    }
    yield_a_bit().await;
}

/// Lets the engine run for ten poll intervals of virtual time.
pub async fn settle() {
    settle_for(10).await;
}

/// Lets a test compile against both `Engine::with_workspaces` signatures: today's `-> Engine` and
/// the possible `-> Result<Engine, E>` (issue #22, leftover 3 of #21).
pub trait Applied {
    fn into_result(self) -> Result<Engine, String>;
}

impl Applied for Engine {
    fn into_result(self) -> Result<Engine, String> {
        Ok(self)
    }
}

impl<E: std::fmt::Display> Applied for Result<Engine, E> {
    fn into_result(self) -> Result<Engine, String> {
        self.map_err(|e| e.to_string())
    }
}
