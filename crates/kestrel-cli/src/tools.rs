//! What `serve` gives the gateway for the Kestrel tools (#42): a [`ToolHost`] over the engine, the
//! agents a run may spawn by name, and the root key that signs capability tokens.
//!
//! * **Agents** are spec files in a directory (`engine.agents_dir`, by default `agents` next to the
//!   database): `kestrel.spawn` with agent `worker` reads `worker.yaml`. Which agents a run may
//!   spawn is still up to its capability; the directory only says what each one is.
//! * **The root key** lives in `root.key` next to the database, made on first start and readable
//!   by its owner only. Every capability token is signed with it, so it must stay the same across
//!   restarts: a new key would refuse the tokens of every run spawned before.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kestrel_core::caps::RootKey;
use kestrel_core::{ChildResult, Engine, JoinMode, JoinOutcome};
use kestrel_gateway::{JoinCall, JoinWait, KestrelTool, SpawnCall, ToolHost, ToolsError};
use kestrel_types::{AgentSpec, RunId};
use serde_json::{Value, json};

use crate::Failure;

/// Where the root key lives for the database at `db`.
pub(crate) fn root_key_path(db: &Path) -> PathBuf {
    db.with_file_name("root.key")
}

/// The default agents directory for the database at `db`.
pub(crate) fn default_agents_dir(db: &Path) -> PathBuf {
    db.with_file_name("agents")
}

/// The root key in `path`, made (owner-only) if there is none. Errors name the file.
pub(crate) fn load_or_create_root_key(path: &Path) -> Result<RootKey, Failure> {
    let named = |e: String| Failure::runtime(format!("{}: {e}", path.display()));
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let bytes = unhex(text.trim()).ok_or_else(|| named("not a root key (hex expected); it signs every capability token, so restore it rather than delete it".into()))?;
            RootKey::from_bytes(&bytes).map_err(|e| named(e.to_string()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let key = RootKey::generate();
            match write_new(path, &hex(&key.to_bytes())) {
                Ok(()) => Ok(key),
                // Another process made it first: use theirs.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => load_or_create_root_key(path),
                Err(e) => Err(named(e.to_string())),
            }
        }
        Err(e) => Err(named(e.to_string())),
    }
}

fn write_new(path: &Path, text: &str) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Agent specs by name, from one directory.
#[derive(Debug, Clone)]
pub(crate) struct AgentDir(pub(crate) PathBuf);

impl AgentDir {
    /// The spec of agent `name`, from `<dir>/<name>.yaml`. The error is for the model to read.
    pub(crate) fn load(&self, name: &str) -> Result<AgentSpec, String> {
        // A name, never a path: letters, digits, `-` and `_`.
        let plain = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !plain {
            return Err(format!(
                "unknown agent `{name}`: agent names are letters, digits, - and _"
            ));
        }
        let path = self.0.join(format!("{name}.yaml"));
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!("unknown agent `{name}`: there is no {name}.yaml in the agents directory")
        })?;
        let spec = AgentSpec::from_yaml(&text)
            .map_err(|e| format!("agent `{name}` has a spec that does not parse: {e}"))?;
        if spec.name != name {
            return Err(format!(
                "{name}.yaml names its agent `{}`, not `{name}`",
                spec.name
            ));
        }
        Ok(spec)
    }
}

/// The Kestrel tools, done by the engine.
pub(crate) struct EngineTools {
    pub(crate) engine: Arc<Engine>,
    pub(crate) agents: AgentDir,
}

#[async_trait]
impl ToolHost for EngineTools {
    async fn tools(&self, run: RunId) -> Result<Vec<KestrelTool>, ToolsError> {
        match self.engine.kestrel_tools(run) {
            Ok(Some(names)) => Ok(names
                .into_iter()
                .filter_map(KestrelTool::from_name)
                .collect()),
            // Ended, or unknown: it can call nothing more.
            Ok(None) => Err(ToolsError::Gone),
            // The store failed: say so, rather than refuse a call that may be allowed.
            Err(_) => Err(ToolsError::Unavailable),
        }
    }

    async fn spawn(&self, run: RunId, call: SpawnCall) -> Result<Value, String> {
        let spec = self.agents.load(&call.agent)?;
        let child = self
            .engine
            .spawn_child(run, &spec, &call.input, call.budget)
            .await
            .map_err(|e| e.to_string())?;
        if !call.wait {
            return Ok(json!({ "child": child.to_string() }));
        }
        let results = self
            .engine
            .join(run, &[child], JoinMode::All, None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "child": child.to_string(), "result": results_json(&results).pop() }))
    }

    async fn join(&self, run: RunId, call: JoinCall) -> Result<Value, String> {
        let mode = match call.wait {
            JoinWait::All => JoinMode::All,
            JoinWait::Any => JoinMode::Any,
        };
        let results = self
            .engine
            .join(run, &call.children, mode, call.timeout)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "children": results_json(&results) }))
    }

    async fn ask_human(&self, run: RunId, question: String) -> Result<String, String> {
        self.engine
            .ask_human(run, question)
            .await
            .map_err(|e| e.to_string())
    }

    async fn denied(&self, run: RunId, tool: &str) {
        if let Err(e) = self.engine.tool_denied(run, tool) {
            eprintln!("kestrel: recording a refused {tool} call for run {run}: {e}");
        }
    }
}

/// Children's results as the model reads them.
fn results_json(results: &[ChildResult]) -> Vec<Value> {
    results
        .iter()
        .map(|r| {
            let mut out = json!({ "child": r.run.to_string(), "spent_usd": r.spent.to_string() });
            let outcome = match &r.outcome {
                JoinOutcome::Completed { summary } => {
                    out["summary"] = json!(summary);
                    "completed"
                }
                JoinOutcome::Failed { reason } => {
                    out["reason"] = json!(reason);
                    "failed"
                }
                JoinOutcome::Cancelled => "cancelled",
                JoinOutcome::TimedOut => "timed_out",
            };
            out["outcome"] = json!(outcome);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use kestrel_types::MicroUsd;

    #[test]
    fn the_root_key_is_made_once_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = root_key_path(&dir.path().join("kestrel.db"));
        assert_eq!(path, dir.path().join("root.key"));
        let made = load_or_create_root_key(&path).unwrap();
        let read = load_or_create_root_key(&path).unwrap();
        assert_eq!(made.to_bytes(), read.to_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
    }

    #[test]
    fn a_root_key_file_that_is_not_one_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.key");
        for bad in ["not hex", "abc", "00ff"] {
            std::fs::write(&path, bad).unwrap();
            let err = load_or_create_root_key(&path).unwrap_err().message;
            assert!(err.contains("root.key"), "{bad}: {err}");
        }
        assert!(unhex("").is_none());
        assert_eq!(unhex("0aFf"), Some(vec![0x0a, 0xff]));
    }

    #[test]
    fn a_root_key_path_that_cannot_be_read_or_written_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be.
        let path = dir.path().join("root.key");
        std::fs::create_dir(&path).unwrap();
        assert!(
            load_or_create_root_key(&path)
                .unwrap_err()
                .message
                .contains("root.key")
        );
        let missing = dir.path().join("no-such-dir").join("root.key");
        assert!(
            load_or_create_root_key(&missing)
                .unwrap_err()
                .message
                .contains("root.key")
        );
    }

    fn spec(name: &str) -> String {
        format!(
            "name: {name}\nruntime: claude-code\nrole: r\n\
             sandbox: {{ tier: auto, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
             budget: {{ usd: \"1\", wall_clock: 1h }}\n"
        )
    }

    #[test]
    fn agents_are_read_by_name_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("worker.yaml"), spec("worker")).unwrap();
        std::fs::write(dir.path().join("liar.yaml"), spec("someone-else")).unwrap();
        std::fs::write(dir.path().join("broken.yaml"), "name: [").unwrap();
        let agents = AgentDir(dir.path().to_path_buf());
        assert_eq!(agents.load("worker").unwrap().name, "worker");
        for (name, says) in [
            ("../worker", "letters, digits"),
            ("", "letters, digits"),
            ("ghost", "no ghost.yaml"),
            ("liar", "names its agent `someone-else`"),
            ("broken", "does not parse"),
        ] {
            let err = agents.load(name).unwrap_err();
            assert!(err.contains(says), "{name}: {err}");
        }
        assert_eq!(
            default_agents_dir(&dir.path().join("kestrel.db")),
            dir.path().join("agents")
        );
    }

    /// The tools over an engine with no sandbox drivers: enough for everything but a live run.
    fn engine_tools(dir: &Path) -> (Arc<kestrel_store::Store>, EngineTools) {
        let store = Arc::new(kestrel_store::Store::open_in_memory().unwrap());
        let engine = Engine::new(
            store.clone(),
            Vec::new(),
            Arc::new(kestrel_agent::adapter_for),
            url::Url::parse("http://127.0.0.1:7070").unwrap(),
            kestrel_core::Limits::default(),
        )
        .with_root_key(RootKey::generate());
        let tools = EngineTools {
            engine: Arc::new(engine),
            agents: AgentDir(dir.to_path_buf()),
        };
        (store, tools)
    }

    fn with_caps(name: &str, caps: &str) -> AgentSpec {
        AgentSpec::from_yaml(&format!(
            "name: {name}\nruntime: claude-code\nrole: r\ncapabilities: {caps}\n\
             sandbox: {{ tier: auto, cpu: 1, memory: 512Mi, workspace: empty-dir }}\n\
             budget: {{ usd: \"1\", wall_clock: 1h }}\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn the_engine_tools_map_the_engine_answers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("worker.yaml"), spec("worker")).unwrap();
        let (store, tools) = engine_tools(dir.path());
        assert_eq!(tools.tools(RunId::new()).await, Err(ToolsError::Gone));
        let lead = tools
            .engine
            .submit(
                with_caps("lead", "{ mcp: [kestrel.join, kestrel.ask_human] }"),
                "x".into(),
            )
            .await
            .unwrap();
        assert_eq!(
            tools.tools(lead).await,
            Ok(vec![KestrelTool::Join, KestrelTool::AskHuman])
        );
        // A queued run cannot ask, spawn, or join a run that is not its child.
        assert!(
            tools
                .ask_human(lead, "q".into())
                .await
                .unwrap_err()
                .contains("not running")
        );
        let spawn = |agent: &str, wait| SpawnCall {
            agent: agent.into(),
            input: "x".into(),
            budget: None,
            wait,
        };
        assert!(
            tools
                .spawn(lead, spawn("ghost", false))
                .await
                .unwrap_err()
                .contains("unknown agent")
        );
        assert!(
            tools
                .spawn(lead, spawn("worker", true))
                .await
                .unwrap_err()
                .contains("running")
        );
        let other = tools
            .engine
            .submit(spec_of("other"), "y".into())
            .await
            .unwrap();
        let join = JoinCall {
            children: vec![other],
            wait: JoinWait::Any,
            timeout: None,
        };
        assert!(
            tools
                .join(lead, join)
                .await
                .unwrap_err()
                .contains("not a child")
        );
        let none = JoinCall {
            children: vec![],
            wait: JoinWait::All,
            timeout: None,
        };
        assert_eq!(
            tools.join(lead, none).await.unwrap(),
            json!({ "children": [] })
        );
        tools.denied(lead, "kestrel.spawn").await;
        let logged = store.events_page(lead, 0, 1_000).unwrap().into_iter().any(|e| {
            matches!(e.kind, kestrel_types::EventKind::AgentLog { ref line } if line.contains("kestrel.spawn"))
        });
        assert!(logged);
    }

    /// Moves a queued run to `Running` the way the engine would.
    fn run(store: &kestrel_store::Store, id: RunId) {
        use kestrel_types::RunStatus::{Provisioning, Queued, Running};
        store.transition(id, &Queued, Provisioning).unwrap();
        store.transition(id, &Provisioning, Running).unwrap();
    }

    #[tokio::test]
    async fn a_running_run_spawns_by_agent_name_joins_and_waits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("worker.yaml"), spec("worker")).unwrap();
        let (store, tools) = engine_tools(dir.path());
        let lead = tools
            .engine
            .submit(
                with_caps(
                    "lead",
                    "{ mcp: [kestrel.spawn, kestrel.join], spawn: { max_children: 4, max_depth: 1, agents: [worker] } }",
                ),
                "x".into(),
            )
            .await
            .unwrap();
        run(&store, lead);
        assert_eq!(
            tools.tools(lead).await,
            Ok(vec![KestrelTool::Spawn, KestrelTool::Join])
        );
        let call = |wait| SpawnCall {
            agent: "worker".into(),
            input: "help".into(),
            budget: Some(kestrel_types::MicroUsd(100_000)),
            wait,
        };
        let spawned = tools.spawn(lead, call(false)).await.unwrap();
        let child: RunId = spawned["child"].as_str().unwrap().parse().unwrap();
        assert_eq!(store.get_run(child).unwrap().unwrap().parent, Some(lead));
        // A join that times out says so, and the child keeps going.
        let join = JoinCall {
            children: vec![child],
            wait: JoinWait::All,
            timeout: Some(std::time::Duration::from_millis(20)),
        };
        let joined = tools.join(lead, join).await.unwrap();
        assert_eq!(joined["children"][0]["outcome"], "timed_out");
        // With wait, the result comes back with the child once it ends.
        let ender = {
            let store = store.clone();
            tokio::spawn(async move {
                loop {
                    let children = store.children(lead).unwrap();
                    if let Some(next) = children.iter().find(|c| c.id != child) {
                        store
                            .transition(
                                next.id,
                                &kestrel_types::RunStatus::Queued,
                                kestrel_types::RunStatus::Cancelled,
                            )
                            .unwrap();
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
        };
        let waited = tools.spawn(lead, call(true)).await.unwrap();
        ender.await.unwrap();
        assert_eq!(waited["result"]["outcome"], "cancelled");
        assert_ne!(waited["child"], spawned["child"]);
    }

    fn spec_of(name: &str) -> AgentSpec {
        AgentSpec::from_yaml(&spec(name)).unwrap()
    }

    #[test]
    fn child_results_read_well() {
        let run = RunId::new();
        let results = [
            ChildResult {
                run,
                outcome: JoinOutcome::Completed {
                    summary: Some("done".into()),
                },
                spent: MicroUsd(250_000),
            },
            ChildResult {
                run,
                outcome: JoinOutcome::Failed {
                    reason: "boom".into(),
                },
                spent: MicroUsd(0),
            },
            ChildResult {
                run,
                outcome: JoinOutcome::Cancelled,
                spent: MicroUsd(0),
            },
            ChildResult {
                run,
                outcome: JoinOutcome::TimedOut,
                spent: MicroUsd(0),
            },
        ];
        let json = results_json(&results);
        assert_eq!(json[0]["outcome"], "completed");
        assert_eq!(json[0]["summary"], "done");
        assert_eq!(json[0]["child"], run.to_string());
        assert_eq!(json[0]["spent_usd"], MicroUsd(250_000).to_string());
        assert_eq!(json[1]["outcome"], "failed");
        assert_eq!(json[1]["reason"], "boom");
        assert_eq!(json[2]["outcome"], "cancelled");
        assert_eq!(json[3]["outcome"], "timed_out");
    }
}
