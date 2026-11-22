//! Every command except `serve`. Each one talks to the engine only through the store.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use kestrel_store::{ControlCommand, RunFilter, RunRow, Store};
use kestrel_types::{
    AgentSpec, ApprovalId, Event, EventKind, RunId, RunStatus, SuspendReason, WorkspaceSpec,
};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{Failure, Output, RunArgs, open};

/// How often `watch` checks for new events.
const WATCH_POLL: Duration = Duration::from_millis(100);

/// Prints one line to stdout. A closed pipe is an error, not a panic.
pub(crate) fn say(line: &str) -> Result<(), Failure> {
    writeln!(std::io::stdout().lock(), "{line}")
        .map_err(|e| Failure::runtime(format!("writing to stdout: {e}")))
}

/// `kestrel run`: checks the agent file, then queues the run the way `Engine::submit` does.
pub(crate) fn run(db: Option<PathBuf>, args: RunArgs, out: Output) -> Result<(), Failure> {
    let mut spec = load_spec(&args.agent)?;
    resolve_repo(&mut spec)?;
    let input = args
        .input
        .ok_or_else(|| Failure::usage("--input is required: the task for the agent"))?;
    let store = open(db)?;
    let id = store.create_run_with_event(&spec, &input)?;
    if out.json {
        say(&json!({ "run_id": id.to_string() }).to_string())?;
    } else {
        say(&id.to_string())?;
    }
    if args.watch {
        watch(&store, id, out)?;
    }
    Ok(())
}

/// Reads and checks an agent file.
fn load_spec(path: &PathBuf) -> Result<AgentSpec, Failure> {
    let bad = |e: &dyn std::fmt::Display| Failure::usage(format!("{}: {e}", path.display()));
    let yaml = std::fs::read_to_string(path).map_err(|e| bad(&e))?;
    AgentSpec::from_yaml(&yaml).map_err(|e| bad(&e))
}

/// A git-clone agent works on the repo around the current dir. Its root is stored as an
/// absolute path, so the engine (running elsewhere) finds the same repo. Fails with a usage
/// error at submit time when there is no repo or no git.
fn resolve_repo(spec: &mut AgentSpec) -> Result<(), Failure> {
    let WorkspaceSpec::GitClone { repo, .. } = &mut spec.sandbox.workspace else {
        return Ok(());
    };
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Failure::usage("git was not found on PATH; a git-clone agent needs git")
            } else {
                Failure::usage(format!("could not run git: {e}"))
            }
        })?;
    let top = String::from_utf8_lossy(&out.stdout);
    let top = top.trim();
    if !out.status.success() || top.is_empty() {
        return Err(Failure::usage(
            "this agent uses a git clone, so run it from inside a git repository",
        ));
    }
    *repo = std::path::absolute(top)
        .map_err(|e| Failure::usage(format!("could not resolve the git repository: {e}")))?;
    Ok(())
}

/// `kestrel watch`: prints events as they arrive until the run reaches a terminal status.
pub(crate) fn watch(store: &Store, id: RunId, out: Output) -> Result<(), Failure> {
    let mut last = 0;
    loop {
        // Read the status before the events: the final status change is written together with
        // its event, so a terminal status here means the last event is already in the log.
        let status = store
            .get_run(id)?
            .ok_or_else(|| Failure::runtime(format!("no such run: {id}")))?
            .status;
        for event in store.events_since(id, last)? {
            last = event.seq;
            say(&format_event(&event, out)?)?;
        }
        if status.is_terminal() {
            return Ok(());
        }
        std::thread::sleep(WATCH_POLL);
    }
}

fn format_event(event: &Event, out: Output) -> Result<String, Failure> {
    if out.json {
        return serde_json::to_string(event).map_err(|e| Failure::runtime(e.to_string()));
    }
    let ts = event.ts;
    Ok(format!(
        "{:02}:{:02}:{:02}  {}",
        ts.hour(),
        ts.minute(),
        ts.second(),
        describe(&event.kind)
    ))
}

/// One event as a short line of text.
fn describe(kind: &EventKind) -> String {
    match kind {
        EventKind::RunCreated { agent, .. } => format!("created run of {agent}"),
        EventKind::StatusChanged { from, to } => {
            format!("status {} -> {}", label(from), label(to))
        }
        EventKind::SandboxProvisioned { driver, sandbox_id } => {
            format!("sandbox {sandbox_id} ({driver})")
        }
        EventKind::AgentMessage { text } => text.clone(),
        EventKind::ToolCall { name, input } => format!("tool {name} {input}"),
        EventKind::ToolResult { name, ok, output } => {
            let how = if *ok { "ok" } else { "failed" };
            format!("tool {name} {how}: {output}")
        }
        EventKind::ModelCall(c) => format!(
            "model {} ({} in, {} out, {})",
            c.model,
            c.tokens_in,
            c.tokens_out,
            c.cost.map_or("unpriced".to_string(), |c| c.to_string())
        ),
        EventKind::BudgetWarning { spent, limit } => {
            format!("budget warning: spent {spent} of {limit}")
        }
        EventKind::ApprovalRequested {
            id,
            action,
            question: false,
        } => format!("approval needed for {action}: kestrel approve {id}"),
        EventKind::ApprovalRequested {
            id,
            action,
            question: true,
        } => format!("question for you: {action} (answer it on the Approvals page, id {id})"),
        EventKind::ApprovalResolved {
            id,
            approved,
            payload,
        } => {
            let how = if *approved { "approved" } else { "rejected" };
            match payload {
                Some(text) => format!("approval {id} {how}: {text}"),
                None => format!("approval {id} {how}"),
            }
        }
        EventKind::AgentLog { line } => format!("log: {line}"),
        EventKind::RunFinished { summary } => match summary {
            Some(s) => format!("finished: {s}"),
            None => "finished".to_string(),
        },
        EventKind::ChildSpawned {
            child,
            agent,
            budget,
        } => format!("spawned {agent} as run {child} with {budget}"),
        EventKind::CapabilitiesDropped {
            tools,
            hosts,
            secrets,
            agents,
        } => {
            let named = [
                ("tools", tools),
                ("hosts", hosts),
                ("secrets", secrets),
                ("agents", agents),
            ];
            let parts: Vec<String> = named
                .iter()
                .filter(|(_, items)| !items.is_empty())
                .map(|(what, items)| format!("{what} {}", items.join(", ")))
                .collect();
            format!("not granted by the parent: {}", parts.join("; "))
        }
    }
}

/// A run status as plain text.
fn label(status: &RunStatus) -> String {
    match status {
        RunStatus::Queued => "Queued".into(),
        RunStatus::Provisioning => "Provisioning".into(),
        RunStatus::Running => "Running".into(),
        RunStatus::Suspended(reason) => match reason {
            SuspendReason::Approval(id) => format!("Suspended (approval {id})"),
            SuspendReason::Budget => "Suspended (budget)".into(),
            SuspendReason::Error(e) => format!("Suspended (error: {e})"),
            SuspendReason::Manual => "Suspended (paused)".into(),
        },
        RunStatus::Completed => "Completed".into(),
        RunStatus::Failed(e) => format!("Failed: {e}"),
        RunStatus::Cancelled => "Cancelled".into(),
    }
}

/// `kestrel ps`: active runs, or every run with `all`, in creation order.
pub(crate) fn ps(store: &Store, all: bool, out: Output) -> Result<(), Failure> {
    let runs: Vec<RunRow> = store
        .list_runs(RunFilter::default())?
        .into_iter()
        .filter(|r| all || !r.status.is_terminal())
        .collect();
    let mut rows = Vec::with_capacity(runs.len());
    for run in runs {
        let spent = store.spent(run.id)?;
        rows.push((run, spent));
    }
    if out.json {
        let mut items = Vec::with_capacity(rows.len());
        for (run, spent) in &rows {
            let created = run
                .created_at
                .format(&Rfc3339)
                .map_err(|e| Failure::runtime(e.to_string()))?;
            items.push(json!({
                "id": run.id.to_string(),
                "agent": run.agent,
                "status": run.status,
                "spent": spent.0,
                "created_at": created,
            }));
        }
        return say(&serde_json::Value::Array(items).to_string());
    }
    say(&format!(
        "{:<36}  {:<16}  {:<14}  {:>12}  AGE",
        "ID", "AGENT", "STATUS", "SPENT"
    ))?;
    let now = OffsetDateTime::now_utc();
    for (run, spent) in &rows {
        say(&format!(
            "{:<36}  {:<16}  {:<14}  {:>12}  {}",
            run.id.to_string(),
            run.agent,
            label(&run.status),
            spent.to_string(),
            age(now - run.created_at)
        ))?;
    }
    Ok(())
}

/// `90s` as `1m`: the largest whole unit.
fn age(d: time::Duration) -> String {
    let s = d.whole_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// `kestrel pause`, `resume`, `cancel` and `budget`: one control command for a known run whose
/// status allows it (or, for cancel, an ended run with a live run below it). A command it does
/// not allow is refused with the reason and never written.
pub(crate) fn steer(
    store: &Store,
    id: RunId,
    cmd: ControlCommand,
    out: Output,
) -> Result<(), Failure> {
    let Some(run) = store.get_run(id)? else {
        return Err(Failure::runtime(format!("no such run: {id}")));
    };
    if let Some(why) = store.command_refusal(&run, &cmd)? {
        return Err(Failure::runtime(format!("run {id}: {why}")));
    }
    send(store, id, cmd, out)
}

/// `kestrel approve`: resolves the approval a suspended run is waiting on.
pub(crate) fn approve(
    store: &Store,
    aid: ApprovalId,
    reject: bool,
    out: Output,
) -> Result<(), Failure> {
    let waiting = RunStatus::Suspended(SuspendReason::Approval(aid));
    let run = store
        .list_runs(RunFilter::default())?
        .into_iter()
        .find(|r| r.status == waiting)
        .ok_or_else(|| Failure::runtime(format!("no run is waiting on approval {aid}")))?;
    let cmd = if reject {
        ControlCommand::Reject(aid)
    } else {
        ControlCommand::Approve(aid)
    };
    send(store, run.id, cmd, out)
}

fn send(store: &Store, id: RunId, cmd: ControlCommand, out: Output) -> Result<(), Failure> {
    let what = match &cmd {
        ControlCommand::Pause => "pause".to_string(),
        ControlCommand::Resume => "resume".to_string(),
        ControlCommand::Cancel => "cancel".to_string(),
        ControlCommand::Approve(_) | ControlCommand::ApproveWith { .. } => "approve".to_string(),
        ControlCommand::Reject(_) | ControlCommand::RejectWith { .. } => "reject".to_string(),
        ControlCommand::SetBudget(usd) => format!("budget {usd}"),
        // Sent by the gateway for kestrel.ask_human, never from the command line.
        ControlCommand::Ask { .. } => "question".to_string(),
    };
    store.push_control(id, cmd)?;
    if out.json {
        say(&json!({ "run_id": id.to_string(), "sent": what }).to_string())
    } else {
        say(&format!("sent {what} to {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_types::{MicroUsd, ModelCallRecord};

    #[test]
    fn describe_covers_every_event() {
        let aid = ApprovalId::new();
        let call = |cost| {
            EventKind::ModelCall(ModelCallRecord {
                run_id: RunId::new(),
                provider: "anthropic".into(),
                model: "m".into(),
                tokens_in: 1,
                tokens_out: 2,
                tokens_cache_read: 0,
                tokens_cache_write: 0,
                cost,
                latency_ms: 1,
                status: 200,
            })
        };
        let cases = [
            (
                EventKind::RunCreated {
                    agent: "a".into(),
                    parent: None,
                },
                "created run of a",
            ),
            (
                EventKind::SandboxProvisioned {
                    driver: "fake".into(),
                    sandbox_id: "s1".into(),
                },
                "sandbox s1 (fake)",
            ),
            (
                EventKind::ToolCall {
                    name: "Bash".into(),
                    input: json!({"cmd": "ls"}),
                },
                "tool Bash {\"cmd\":\"ls\"}",
            ),
            (
                EventKind::ToolResult {
                    name: "Bash".into(),
                    ok: true,
                    output: "x".into(),
                },
                "tool Bash ok: x",
            ),
            (
                EventKind::ToolResult {
                    name: "Bash".into(),
                    ok: false,
                    output: "x".into(),
                },
                "tool Bash failed: x",
            ),
            (call(Some(MicroUsd(5))), "model m (1 in, 2 out, $0.000005)"),
            (call(None), "model m (1 in, 2 out, unpriced)"),
            (
                EventKind::BudgetWarning {
                    spent: MicroUsd(1),
                    limit: MicroUsd(2),
                },
                "budget warning: spent $0.000001 of $0.000002",
            ),
            (
                EventKind::ApprovalResolved {
                    id: aid,
                    approved: true,
                    payload: None,
                },
                "approved",
            ),
            (
                EventKind::ApprovalResolved {
                    id: aid,
                    approved: false,
                    payload: Some("no".into()),
                },
                "rejected: no",
            ),
            (EventKind::AgentLog { line: "l".into() }, "log: l"),
            (EventKind::RunFinished { summary: None }, "finished"),
            (
                EventKind::ChildSpawned {
                    child: RunId::new(),
                    agent: "fixer".into(),
                    budget: MicroUsd(500_000),
                },
                "spawned fixer as run ",
            ),
            (
                EventKind::CapabilitiesDropped {
                    tools: vec!["a".into(), "b".into()],
                    hosts: vec![],
                    secrets: vec![],
                    agents: vec!["x".into()],
                },
                "not granted by the parent: tools a, b; agents x",
            ),
        ];
        for (kind, want) in cases {
            let got = describe(&kind);
            assert!(got.contains(want), "{kind:?}: {got}");
        }
        let asked = describe(&EventKind::ApprovalRequested {
            id: aid,
            action: "push".into(),
            question: false,
        });
        assert!(asked.contains(&format!("kestrel approve {aid}")), "{asked}");
        let question = describe(&EventKind::ApprovalRequested {
            id: aid,
            action: "Which branch?".into(),
            question: true,
        });
        assert!(
            question.starts_with("question for you: Which branch?"),
            "{question}"
        );
        assert!(question.contains(&aid.to_string()), "{question}");
    }

    #[test]
    fn label_covers_every_status() {
        let aid = ApprovalId::new();
        let cases = [
            (RunStatus::Provisioning, "Provisioning".to_string()),
            (
                RunStatus::Suspended(SuspendReason::Approval(aid)),
                format!("Suspended (approval {aid})"),
            ),
            (
                RunStatus::Suspended(SuspendReason::Budget),
                "Suspended (budget)".into(),
            ),
            (
                RunStatus::Suspended(SuspendReason::Error("x".into())),
                "Suspended (error: x)".into(),
            ),
            (
                RunStatus::Suspended(SuspendReason::Manual),
                "Suspended (paused)".into(),
            ),
            (RunStatus::Failed("boom".into()), "Failed: boom".into()),
            (RunStatus::Cancelled, "Cancelled".into()),
        ];
        for (status, want) in cases {
            assert_eq!(label(&status), want);
        }
    }

    #[test]
    fn age_uses_the_largest_whole_unit() {
        let s = time::Duration::seconds;
        assert_eq!(age(s(-5)), "0s");
        assert_eq!(age(s(59)), "59s");
        assert_eq!(age(s(90)), "1m");
        assert_eq!(age(s(7200)), "2h");
        assert_eq!(age(s(3 * 86400)), "3d");
    }
}
