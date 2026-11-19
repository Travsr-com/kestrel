//! A fake `CommandRunner` that records every argv and never starts a process.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use kestrel_sandbox::{
    CommandOutput, CommandRunner, LaunchSpec, NetworkPolicy, OutputLine, SpawnedProcess,
};

/// Container id the fake prints on stdout for every successful call.
pub const ID: &str = "abc123";

type Responder = dyn Fn(&[String]) -> CommandOutput + Send + Sync;

pub struct FakeRunner {
    runs: Mutex<Vec<Vec<String>>>,
    spawns: Mutex<Vec<Vec<String>>>,
    respond: Box<Responder>,
    lines: Vec<OutputLine>,
    /// Exit code every spawned process reports.
    exit: i32,
}

impl FakeRunner {
    /// Every call succeeds and prints `abc123`.
    pub fn ok() -> Arc<Self> {
        Self::with(|_| CommandOutput {
            status: 0,
            stdout: format!("{ID}\n"),
            stderr: String::new(),
        })
    }

    /// Every `run` call answers with `respond(argv)`.
    pub fn with(respond: impl Fn(&[String]) -> CommandOutput + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            runs: Mutex::new(Vec::new()),
            spawns: Mutex::new(Vec::new()),
            respond: Box::new(respond),
            lines: Vec::new(),
            exit: 0,
        })
    }

    /// Every call succeeds; `spawn` streams `lines` in order.
    pub fn streaming(lines: Vec<OutputLine>) -> Arc<Self> {
        Arc::new(Self {
            runs: Mutex::new(Vec::new()),
            spawns: Mutex::new(Vec::new()),
            respond: Box::new(|_| CommandOutput {
                status: 0,
                stdout: format!("{ID}\n"),
                stderr: String::new(),
            }),
            lines,
            exit: 0,
        })
    }

    /// Every call succeeds; `spawn` streams `lines` and then exits with `code`.
    pub fn exiting(lines: Vec<OutputLine>, code: i32) -> Arc<Self> {
        let mut runner = Self::streaming(lines);
        if let Some(r) = Arc::get_mut(&mut runner) {
            r.exit = code;
        }
        runner
    }

    /// Every argv passed to `run`, in order.
    pub fn runs(&self) -> Vec<Vec<String>> {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every argv passed to `spawn`, in order.
    pub fn spawns(&self) -> Vec<Vec<String>> {
        self.spawns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every argv passed to `run` or `spawn`.
    pub fn all_calls(&self) -> Vec<Vec<String>> {
        let mut all = self.runs();
        all.extend(self.spawns());
        all
    }
}

#[async_trait]
impl CommandRunner for FakeRunner {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
        Ok((self.respond)(argv))
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<SpawnedProcess> {
        self.spawns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
        Ok(SpawnedProcess {
            output: Box::pin(futures::stream::iter(self.lines.clone())),
            exit: Box::pin(futures::future::ready(Ok(self.exit))),
        })
    }
}

/// `vec!["a", "b"]` as `Vec<String>`.
pub fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

/// The spec used by the command-line snapshots.
pub fn spec() -> LaunchSpec {
    let mut env = BTreeMap::new();
    env.insert("RUST_LOG".to_string(), "info".to_string());
    env.insert("TASK".to_string(), "review PR 42".to_string());
    LaunchSpec {
        image: "alpine:3.20".to_string(),
        argv: s(&["sh", "-c", "echo hi"]),
        env,
        workspace: Some(PathBuf::from("/tmp/kestrel/ws-1")),
        cpu_millis: 1500,
        memory_mb: 512,
        network: NetworkPolicy::None,
    }
}
