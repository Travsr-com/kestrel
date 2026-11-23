//! `kestrel fetch <run-id>`: brings a finished run's branch `kestrel/run-<id>` home into its source
//! repository (#93), as the engine does by itself when a run ends. For a run whose automatic
//! fetch failed, or that ended before Kestrel fetched on its own.
//!
//! It reads the run's spec from the store and its clone from `<db dir>/workspaces`, where
//! `kestrel serve` keeps it, and only creates or fast-forwards the run branch in the source repo.
//! Exit 0 when the branch is there afterwards or there was nothing to fetch; 1 when the run is
//! unknown, has not ended, has no clone, or git fails.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kestrel_core::{BroughtBack, Workspaces};
use kestrel_sandbox::ProcessRunner;
use kestrel_types::{AgentSpec, RunId, WorkspaceSpec};
use serde_json::json;

use crate::commands::{label, say};
use crate::{Failure, Output, data_dir_of, db_path, open, workspace_root};

/// How many characters of a commit id the text output shows.
const SHORT: usize = 12;

/// `kestrel fetch`: brings `id`'s branch home and says what happened.
pub(crate) fn fetch(db: Option<PathBuf>, id: RunId, out: Output) -> Result<(), Failure> {
    let db = db_path(db)?;
    let store = open(Some(db.clone()))?;
    let run = store
        .get_run(id)?
        .ok_or_else(|| Failure::runtime(format!("no such run: {id}")))?;
    if !run.status.is_terminal() {
        return Err(Failure::runtime(format!(
            "run {id} has not ended ({}); its branch comes home by itself when it ends",
            label(&run.status)
        )));
    }
    let repo = repo_of(id, store.run_spec(id)?)?;
    let workspaces = Workspaces::new(workspace_root(&data_dir_of(&db)?), Arc::new(ProcessRunner));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::runtime(format!("starting the async runtime: {e}")))?;
    let result = runtime
        .block_on(workspaces.bring_back(id, &repo))
        .map_err(|e| {
            Failure::runtime(format!(
                "fetching {} into {}: {e}",
                Workspaces::branch_for(id),
                repo.display()
            ))
        })?;
    if result == BroughtBack::NoClone {
        return Err(Failure::runtime(format!(
            "run {id} has no workspace at {}: it ended before its clone was made, or the clone \
             was removed",
            workspaces.path_for(id).display()
        )));
    }
    report(id, &repo, &result, out)
}

/// The source repo of a run's `git-clone` workspace.
fn repo_of(id: RunId, spec: Option<AgentSpec>) -> Result<PathBuf, Failure> {
    match spec.map(|s| s.sandbox.workspace) {
        Some(WorkspaceSpec::GitClone { repo, .. }) => Ok(repo),
        Some(_) => Err(Failure::runtime(format!(
            "run {id} has no git-clone workspace, so it has no branch to fetch"
        ))),
        None => Err(Failure::runtime(format!(
            "run {id} has no stored agent spec, so its repository is unknown"
        ))),
    }
}

/// Prints what [`Workspaces::bring_back`] did: one JSON object, or one line of text.
fn report(id: RunId, repo: &Path, result: &BroughtBack, out: Output) -> Result<(), Failure> {
    let branch = result
        .branch()
        .map_or_else(|| Workspaces::branch_for(id), str::to_string);
    if out.json {
        return say(&json!({
            "run_id": id.to_string(),
            "branch": branch,
            "repo": repo.display().to_string(),
            "result": result.name(),
            "commit": result.commit(),
        })
        .to_string());
    }
    let short = result
        .commit()
        .map(|c| c.get(..SHORT).unwrap_or(c))
        .unwrap_or_default();
    let repo = repo.display();
    say(&match result {
        BroughtBack::Fetched { .. } => format!("fetched {branch} into {repo} at {short}"),
        BroughtBack::AlreadyFetched { .. } => {
            format!("{branch} is already in {repo} and holds {short}; nothing to fetch")
        }
        BroughtBack::NothingNew { .. } => format!(
            "run {id} made no new commits ({branch} is at {short}, which {repo} already has); \
             nothing to fetch"
        ),
        BroughtBack::NoBranch { .. } | BroughtBack::NoClone => {
            format!("the clone of run {id} has no branch {branch}; nothing to fetch")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_without_a_stored_spec_has_no_known_repo() {
        let id = RunId::new();
        let err = repo_of(id, None).unwrap_err();
        assert!(
            err.message.contains("no stored agent spec"),
            "{}",
            err.message
        );
    }
}
