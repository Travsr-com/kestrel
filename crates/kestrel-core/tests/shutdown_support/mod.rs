//! Helpers for the #19 tests: a [`Probe`] driver that wraps the shared `FakeDriver` and adds what
//! the shutdown and sweep tests need (completed destroys, slow or failing destroys, `leftovers`).
//! The shared fakes in `tests/common` are untouched.

#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use kestrel_core::{Engine, Limits};
use kestrel_sandbox::{
    ExitStatus, LaunchSpec, OutputStream, SandboxDriver, SandboxError, SandboxHandle,
};
use tokio::sync::watch;
use url::Url;

use crate::common::{GATEWAY, Harness, POLL, adapter_factory};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct State {
    inner: crate::common::FakeDriver,
    created: Mutex<Vec<String>>,
    /// Ids whose destroy ran to completion (a destroy that was dropped half way is not here).
    destroyed: Mutex<Vec<String>>,
    /// Ids of every destroy call, completed or not.
    attempts: Mutex<Vec<String>>,
    /// Destroys wait until this is true.
    open: watch::Sender<bool>,
    /// Every destroy fails with this text.
    fail_all: Mutex<Option<String>>,
    /// Destroys of these ids fail.
    fail_ids: Mutex<Vec<String>>,
    leftovers: Mutex<Result<Vec<String>, String>>,
}

/// A driver named `fake` that delegates to a `FakeDriver` and records what finished.
#[derive(Clone)]
pub struct Probe(Arc<State>);

impl Probe {
    pub fn new(inner: crate::common::FakeDriver) -> Probe {
        Probe(Arc::new(State {
            inner,
            created: Mutex::default(),
            destroyed: Mutex::default(),
            attempts: Mutex::default(),
            open: watch::channel(true).0,
            fail_all: Mutex::default(),
            fail_ids: Mutex::default(),
            leftovers: Mutex::new(Ok(Vec::new())),
        }))
    }

    /// Later destroys block until [`Probe::release_destroys`].
    pub fn hold_destroys(&self) {
        self.0.open.send_replace(false);
    }

    pub fn release_destroys(&self) {
        self.0.open.send_replace(true);
    }

    /// Every destroy fails with `msg`.
    pub fn fail_destroys(&self, msg: &str) {
        *lock(&self.0.fail_all) = Some(msg.to_string());
    }

    /// Destroys of `ids` fail.
    pub fn fail_destroys_of(&self, ids: &[&str]) {
        lock(&self.0.fail_ids).extend(ids.iter().map(|i| (*i).to_string()));
    }

    /// What `leftovers` returns.
    pub fn set_leftovers(&self, ids: &[&str]) {
        *lock(&self.0.leftovers) = Ok(ids.iter().map(|i| (*i).to_string()).collect());
    }

    pub fn fail_leftovers(&self, msg: &str) {
        *lock(&self.0.leftovers) = Err(msg.to_string());
    }

    /// Sandboxes created whose destroy never completed.
    pub fn unreleased(&self) -> Vec<String> {
        let done = lock(&self.0.destroyed).clone();
        lock(&self.0.created)
            .iter()
            .filter(|id| !done.contains(id))
            .cloned()
            .collect()
    }

    /// Ids whose destroy completed, in order.
    pub fn destroyed(&self) -> Vec<String> {
        lock(&self.0.destroyed).clone()
    }

    /// Ids of every destroy call, in order.
    pub fn attempts(&self) -> Vec<String> {
        lock(&self.0.attempts).clone()
    }
}

#[async_trait]
impl SandboxDriver for Probe {
    fn name(&self) -> &'static str {
        self.0.inner.name()
    }

    fn is_strong(&self) -> bool {
        true
    }

    async fn create(&self, spec: &LaunchSpec) -> kestrel_sandbox::Result<SandboxHandle> {
        let h = self.0.inner.create(spec).await?;
        lock(&self.0.created).push(h.id.clone());
        Ok(h)
    }

    async fn start(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<OutputStream> {
        self.0.inner.start(h).await
    }

    async fn wait(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<ExitStatus> {
        self.0.inner.wait(h).await
    }

    async fn stop(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        self.0.inner.stop(h).await
    }

    async fn destroy(&self, h: &SandboxHandle) -> kestrel_sandbox::Result<()> {
        lock(&self.0.attempts).push(h.id.clone());
        let mut open = self.0.open.subscribe();
        // Like a slow `docker rm`: the future may be dropped while it waits.
        let _ = open.wait_for(|o| *o).await;
        let failure = lock(&self.0.fail_all).clone().or_else(|| {
            lock(&self.0.fail_ids)
                .contains(&h.id)
                .then(|| format!("cannot remove {}", h.id))
        });
        if let Some(stderr) = failure {
            return Err(SandboxError::CommandFailed {
                program: "fake".into(),
                code: 1,
                stderr,
            });
        }
        self.0.inner.destroy(h).await?;
        lock(&self.0.destroyed).push(h.id.clone());
        Ok(())
    }

    async fn leftovers(&self) -> kestrel_sandbox::Result<Vec<SandboxHandle>> {
        match lock(&self.0.leftovers).clone() {
            Ok(ids) => Ok(ids.into_iter().map(|id| SandboxHandle { id }).collect()),
            Err(stderr) => Err(SandboxError::CommandFailed {
                program: "fake".into(),
                code: 1,
                stderr,
            }),
        }
    }
}

/// A harness whose engine uses a [`Probe`] around the harness's `FakeDriver` (so `h.driver`
/// scripts the sandboxes and the probe watches them).
pub fn harness(max: usize) -> (Harness, Probe) {
    let mut h = Harness::new(max);
    let probe = Probe::new(h.driver.clone());
    use_probe(&mut h, &probe, max);
    (h, probe)
}

/// Like [`harness`], on the database at `db`.
pub fn harness_on(db: &std::path::Path, max: usize) -> (Harness, Probe) {
    let mut h = Harness::on(db, max);
    let probe = Probe::new(h.driver.clone());
    use_probe(&mut h, &probe, max);
    (h, probe)
}

fn use_probe(h: &mut Harness, probe: &Probe, max: usize) {
    h.engine = Arc::new(Engine::new(
        h.store.clone(),
        vec![Box::new(probe.clone())],
        adapter_factory(h.adapters.clone()),
        Url::parse(GATEWAY).expect("gateway url"),
        Limits {
            max_concurrent_runs: max,
            poll_interval: POLL,
        },
    ));
}
