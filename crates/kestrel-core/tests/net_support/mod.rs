//! Helpers for the #10 tests: a [`NetDriver`] that wraps the shared `FakeDriver` and adds
//! `prepare_network`, `remove_network` and `leftover_networks`, and logs every call in order.
//! The shared fakes in `tests/common` are untouched.

#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use kestrel_core::{Engine, Limits};
use kestrel_sandbox::{
    ExitStatus, LaunchSpec, OutputStream, PreparedNetwork, SandboxDriver, SandboxError,
    SandboxHandle,
};
use kestrel_types::RunId;
use url::Url;

use crate::common::{FakeDriver, GATEWAY, Harness, POLL, adapter_factory};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct State {
    /// Every call, in order: `prepare:<run>`, `create`, `destroy:<id>`, `remove:<network>`.
    log: Mutex<Vec<String>>,
    next_ip: Mutex<u8>,
    prepare_failure: Mutex<Option<String>>,
    remove_failure: Mutex<Option<String>>,
    leftovers: Mutex<Result<Vec<String>, String>>,
}

impl Default for State {
    fn default() -> Self {
        State {
            log: Mutex::default(),
            next_ip: Mutex::default(),
            prepare_failure: Mutex::default(),
            remove_failure: Mutex::default(),
            leftovers: Mutex::new(Ok(Vec::new())),
        }
    }
}

/// A driver named `fake` with per-run networks.
#[derive(Clone)]
pub struct NetDriver {
    inner: FakeDriver,
    state: Arc<State>,
}

/// The network name `prepare_network` hands out for a run.
pub fn network_for(run: RunId) -> String {
    let id = run.to_string();
    format!("net-{}", &id[id.len() - 12..])
}

impl NetDriver {
    pub fn new(inner: FakeDriver) -> NetDriver {
        NetDriver {
            inner,
            state: Arc::new(State::default()),
        }
    }

    pub fn log(&self) -> Vec<String> {
        lock(&self.state.log).clone()
    }

    /// Position of the first log entry that starts with `prefix`.
    pub fn position(&self, prefix: &str) -> Option<usize> {
        self.log().iter().position(|l| l.starts_with(prefix))
    }

    /// Every network passed to `remove_network`, in order.
    pub fn removed(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|l| l.strip_prefix("remove:").map(String::from))
            .collect()
    }

    /// The next `prepare_network` fails with `stderr` (and, like the real driver, leaves nothing
    /// behind).
    pub fn fail_prepare(&self, stderr: &str) {
        *lock(&self.state.prepare_failure) = Some(stderr.to_string());
    }

    /// Every `remove_network` fails with `stderr`.
    pub fn fail_remove(&self, stderr: &str) {
        *lock(&self.state.remove_failure) = Some(stderr.to_string());
    }

    pub fn set_leftover_networks(&self, names: &[&str]) {
        *lock(&self.state.leftovers) = Ok(names.iter().map(|n| (*n).to_string()).collect());
    }

    pub fn fail_leftover_networks(&self, stderr: &str) {
        *lock(&self.state.leftovers) = Err(stderr.to_string());
    }

    fn note(&self, line: String) {
        lock(&self.state.log).push(line);
    }
}

fn failure(stderr: String) -> SandboxError {
    SandboxError::CommandFailed {
        program: "fake".into(),
        code: 1,
        stderr,
    }
}

#[async_trait]
impl SandboxDriver for NetDriver {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn is_strong(&self) -> bool {
        true
    }

    async fn create(&self, spec: &LaunchSpec) -> kestrel_sandbox::Result<SandboxHandle> {
        self.note("create".into());
        self.inner.create(spec).await
    }

    async fn start(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<OutputStream> {
        self.inner.start(h).await
    }

    async fn wait(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<ExitStatus> {
        self.inner.wait(h).await
    }

    async fn stop(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.inner.stop(h).await
    }

    async fn destroy(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.note(format!("destroy:{}", h.id));
        self.inner.destroy(h).await
    }

    async fn leftovers(&self) -> kestrel_sandbox::Result<Vec<SandboxHandle>> {
        Ok(Vec::new())
    }

    async fn prepare_network(
        &self,
        run: RunId,
    ) -> kestrel_sandbox::Result<Option<PreparedNetwork>> {
        self.note(format!("prepare:{run}"));
        if let Some(stderr) = lock(&self.state.prepare_failure).take() {
            return Err(failure(stderr));
        }
        let mut n = lock(&self.state.next_ip);
        *n += 1;
        Ok(Some(PreparedNetwork {
            network: network_for(run),
            gateway_host: IpAddr::V4(Ipv4Addr::new(10, 77, 0, *n)),
        }))
    }

    async fn remove_network(&self, network: &str) -> kestrel_sandbox::Result<()> {
        self.note(format!("remove:{network}"));
        match lock(&self.state.remove_failure).clone() {
            Some(stderr) => Err(failure(stderr)),
            None => Ok(()),
        }
    }

    async fn leftover_networks(&self) -> kestrel_sandbox::Result<Vec<String>> {
        match lock(&self.state.leftovers).clone() {
            Ok(names) => Ok(names),
            Err(stderr) => Err(failure(stderr)),
        }
    }
}

/// A harness whose engine uses a [`NetDriver`] around the harness's `FakeDriver`.
pub fn harness(max: usize) -> (Harness, NetDriver) {
    let mut h = Harness::new(max);
    let net = NetDriver::new(h.driver.clone());
    h.engine = Arc::new(Engine::new(
        h.store.clone(),
        vec![Box::new(net.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("gateway url"),
        Limits {
            max_concurrent_runs: max,
            poll_interval: POLL,
        },
    ));
    (h, net)
}
