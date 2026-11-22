//! Bringing a run's branch home when the run ends (#93).
//!
//! A run with a `git-clone` workspace works on branch `kestrel/run-<id>` in its own clone. Once
//! the run has ended (completed, failed or cancelled) and its sandbox is gone, the engine brings
//! that branch home into the source repository with [`Workspaces::bring_back`]: only when the
//! clone has commits the source does not, and only the ref `refs/heads/kestrel/run-<id>`.
//!
//! The outcome of the run never depends on it. A fetch is recorded in the run's log
//! (`kestrel: fetched branch ...`), a failure as a warning that names `kestrel fetch <run-id>`,
//! which tries again. A run with nothing to bring back records nothing.

use std::path::Path;

use kestrel_types::{AgentSpec, RunId, WorkspaceSpec};

use crate::Inner;
use crate::workspace::{BroughtBack, WorkspaceError};

/// How many characters of a commit id a log line shows.
const SHORT: usize = 12;

impl Inner {
    /// Brings `run`'s branch home, if the engine has workspaces and `spec` has a `git-clone`
    /// one. The log line to record for it, if any.
    pub(crate) async fn bring_branch_home(&self, run: RunId, spec: &AgentSpec) -> Option<String> {
        let workspaces = self.workspaces.as_ref()?;
        let WorkspaceSpec::GitClone { repo, .. } = &spec.sandbox.workspace else {
            return None;
        };
        match workspaces.bring_back(run, repo).await {
            Ok(BroughtBack::Fetched { branch, commit }) => {
                Some(fetched_line(&branch, &commit, repo))
            }
            Ok(_) => None,
            Err(e) => Some(failed_line(run, repo, &e)),
        }
    }
}

/// `kestrel: fetched branch <branch> into <repo> at <commit>`.
fn fetched_line(branch: &str, commit: &str, repo: &Path) -> String {
    let short = commit.get(..SHORT).unwrap_or(commit);
    format!(
        "kestrel: fetched branch {branch} into {} at {short}",
        repo.display()
    )
}

/// The warning for a fetch that failed, naming the command that tries again.
fn failed_line(run: RunId, repo: &Path, e: &WorkspaceError) -> String {
    let branch = crate::Workspaces::branch_for(run);
    format!(
        "kestrel: warning: branch {branch} was not fetched into {}: {e}. \
         The run's outcome is unchanged; `kestrel fetch {run}` tries again",
        repo.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fetched_line_shows_a_short_commit_and_a_short_id_stays_whole() {
        let repo = Path::new("/srv/repo");
        let line = fetched_line("kestrel/run-abc", &"a".repeat(40), repo);
        assert!(line.ends_with(&format!("at {}", "a".repeat(12))), "{line}");
        assert!(fetched_line("b", "abc", repo).ends_with("at abc"));
    }
}
