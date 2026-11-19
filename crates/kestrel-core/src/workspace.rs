//! Per-run workspaces, mounted at `/workspace` inside the sandbox.
//!
//! Every run gets its own directory at `<root>/<run id>`:
//!
//! * `EmptyDir`: a fresh, empty directory.
//! * `GitWorktree { repo }`: a new git worktree of `repo` on its own branch
//!   (`kestrel/run-<short id>`, see [`Workspaces::branch_for`]), checked out from `HEAD`.
//!
//! `prepare` is idempotent: if the run's directory already exists (for example after an engine
//! restart), it is reused as is and no git command runs. Workspaces are kept after the run
//! ends; cleanup is a separate command.
//!
//! All git calls go through a [`CommandRunner`], so unit tests use a fake and never spawn git.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kestrel_sandbox::{CommandRunner, validate_workspace_path};
use kestrel_types::{RunId, WorkspaceSpec};

/// Why a workspace could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The `git-worktree` repo path is not inside a git repository.
    #[error("`{}` is not a git repository", repo.display())]
    NotAGitRepo { repo: PathBuf },
    /// A git command exited non-zero. `stderr` is git's own error text.
    #[error("`{}` exited with code {status}: {stderr}", argv.join(" "))]
    Git {
        argv: Vec<String>,
        status: i32,
        stderr: String,
    },
    /// A filesystem operation, or starting git itself, failed.
    #[error("i/o error at `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The workspace path would fail the sandbox's mount validation: it is relative, not
    /// UTF-8, or contains `,` `=` `"` or a newline.
    #[error(
        "invalid workspace path `{path}`: must be absolute UTF-8 without `,` `=` `\"` or newlines"
    )]
    InvalidPath { path: String },
}

/// Creates and finds per-run workspaces under one root directory.
pub struct Workspaces {
    root: PathBuf,
    runner: Arc<dyn CommandRunner>,
}

impl Workspaces {
    /// `root` is usually `<kestrel data dir>/workspaces`. It is validated by `prepare`.
    pub fn new(root: PathBuf, runner: Arc<dyn CommandRunner>) -> Self {
        Self { root, runner }
    }

    /// The workspace directory for `run`: `<root>/<run id>`. Pure, touches nothing on disk.
    pub fn path_for(&self, run: RunId) -> PathBuf {
        self.root.join(run.to_string())
    }

    /// The git branch for `run`: `kestrel/run-<last 12 hex chars of the run id>`.
    /// The tail is used because a v7 id starts with its timestamp, so runs created close
    /// together share a prefix but not a tail.
    pub fn branch_for(run: RunId) -> String {
        let id = run.to_string();
        let tail = id.get(id.len().saturating_sub(12)..).unwrap_or(&id);
        format!("kestrel/run-{tail}")
    }

    /// Makes sure `run` has a workspace and returns its absolute path.
    /// Same run, same path; an existing workspace is reused without running git.
    pub async fn prepare(
        &self,
        run: RunId,
        spec: &WorkspaceSpec,
    ) -> Result<PathBuf, WorkspaceError> {
        let path = self.path_for(run);
        let text = validate_workspace_path(&path)
            .map_err(|_| WorkspaceError::InvalidPath {
                path: path.display().to_string(),
            })?
            .to_string();
        if tokio::fs::try_exists(&path).await.map_err(io(&path))? {
            return Ok(path);
        }
        match spec {
            WorkspaceSpec::EmptyDir => create_dir(&path).await?,
            WorkspaceSpec::GitWorktree { repo } => {
                let git = |args: &[&str]| -> Vec<String> {
                    let repo = repo.to_string_lossy().into_owned();
                    ["git", "-C", &repo]
                        .iter()
                        .chain(args)
                        .map(|a| (*a).to_string())
                        .collect()
                };
                let check = self.git(&git(&["rev-parse", "--git-dir"]), repo).await?;
                if check.status != 0 {
                    return Err(WorkspaceError::NotAGitRepo { repo: repo.clone() });
                }
                create_dir(&self.root).await?;
                let branch = Self::branch_for(run);
                let argv = git(&["worktree", "add", "-b", &branch, &text, "HEAD"]);
                let out = self.git(&argv, repo).await?;
                if out.status != 0 {
                    return Err(WorkspaceError::Git {
                        argv,
                        status: out.status,
                        stderr: out.stderr.trim_end().to_string(),
                    });
                }
            }
        }
        Ok(path)
    }

    /// Runs one git command; failing to start git is an `Io` error at `repo`.
    async fn git(
        &self,
        argv: &[String],
        repo: &Path,
    ) -> Result<kestrel_sandbox::CommandOutput, WorkspaceError> {
        self.runner.run(argv).await.map_err(io(repo))
    }
}

async fn create_dir(path: &Path) -> Result<(), WorkspaceError> {
    tokio::fs::create_dir_all(path).await.map_err(io(path))
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> WorkspaceError + '_ {
    move |source| WorkspaceError::Io {
        path: path.to_path_buf(),
        source,
    }
}
