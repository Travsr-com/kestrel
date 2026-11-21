//! Per-run workspaces, mounted at `/workspace` inside the sandbox.
//!
//! Every run gets its own directory at `<root>/<run id>`:
//!
//! * `EmptyDir`: a fresh, empty directory.
//! * `GitClone { repo, depth }`: a full, self-contained `git clone --local --no-hardlinks` of
//!   `repo`, on its own branch (`kestrel/run-<short id>`, see [`Workspaces::branch_for`]).
//!   `depth` makes it shallow. The clone is the agent's to rewrite, so the host runs git inside
//!   it only before its sandbox exists (the checkout, and [`Workspaces::head`]);
//!   [`Workspaces::fetch_back`] pulls the branch home from the source repo instead.
//!
//! `prepare` is idempotent: if the run's directory already exists (for example after an engine
//! restart), it is reused as is and no git command runs. Workspaces are kept after the run
//! ends; cleanup is a separate command.
//!
//! [`Workspaces::head`] reads the commit a fresh clone is at, before its sandbox exists, and
//! [`clone_at`] makes a host-only copy of a repo at a commit (Travsr indexes such a copy, never
//! the workspace).
//!
//! All git calls go through a [`CommandRunner`], so unit tests use a fake and never spawn git.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kestrel_sandbox::{CommandRunner, EnvScrub, validate_workspace_path};
use kestrel_types::{RunId, WorkspaceSpec};

/// Why a workspace could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The `git-clone` repo path is not inside a git repository.
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
    /// The repo path is not valid UTF-8, so it cannot be passed to git as a text argument.
    #[error("repo path `{}` is not valid UTF-8", repo.display())]
    NonUtf8Repo { repo: PathBuf },
    /// The run's directory already exists but is not a usable workspace: it is a regular file,
    /// or (for a git clone) it has no `.git` entry. `reason` says which.
    #[error("existing workspace `{}` is not usable: {reason}", path.display())]
    BadExisting { path: PathBuf, reason: String },
    /// The workspace path would fail the sandbox's mount validation: it is relative, not
    /// UTF-8, or contains `,` `=` `"` or a newline.
    #[error(
        "invalid workspace path `{path}`: must be absolute UTF-8 without `,` `=` `\"` or newlines"
    )]
    InvalidPath { path: String },
    /// `prepare` was given a relative `git-clone` repo path; it is refused before any git call.
    #[error("git-clone repo `{}` must be an absolute path", repo.display())]
    RelativeRepo { repo: PathBuf },
    /// `<clone>/.git` is missing, a file (gitfile) or a symlink, so `fetch_back` refuses to
    /// fetch from the clone. `clone` is the clone directory.
    #[error("`.git` in `{}` is not a real directory; refusing to fetch from it", clone.display())]
    GitDirNotADirectory { clone: PathBuf },
    /// The clone has an `objects/info/alternates` or `http-alternates` entry, which could make
    /// the fetch read objects from another repository, so `fetch_back` refuses.
    #[error("clone `{}` has object alternates; refusing to fetch from it", clone.display())]
    AlternatesPresent { clone: PathBuf },
    /// Not a full commit id (40 or 64 lowercase hex characters). `text` is cut to 80 bytes.
    #[error("`{text}` is not a commit id")]
    BadCommit { text: String },
}

/// The environment every host git call runs with: removes `GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_CONFIG_*`, `GIT_SSH*` and `GIT_EXEC_PATH`; sets `GIT_CONFIG_NOSYSTEM=1` and
/// `GIT_TERMINAL_PROMPT=0`.
pub fn git_env_scrub() -> EnvScrub {
    let names = |list: &[&str]| list.iter().map(|n| (*n).to_string()).collect();
    EnvScrub {
        remove: names(&[
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_CONFIG_*",
            "GIT_SSH*",
            "GIT_EXEC_PATH",
            "GIT_TRACE*",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_INDEX_FILE",
            "GIT_NAMESPACE",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
            "GIT_ALLOW_PROTOCOL",
            "GIT_PROTOCOL_FROM_USER",
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "GIT_PROXY_COMMAND",
            "GIT_EXTERNAL_DIFF",
            "GIT_PAGER",
        ]),
        set: vec![
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ],
    }
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
        if let WorkspaceSpec::GitClone { repo, .. } = spec
            && repo.is_relative()
        {
            return Err(WorkspaceError::RelativeRepo { repo: repo.clone() });
        }
        let path = self.path_for(run);
        let text = validate_workspace_path(&path)
            .map_err(|_| WorkspaceError::InvalidPath {
                path: path.display().to_string(),
            })?
            .to_string();
        if tokio::fs::try_exists(&path).await.map_err(io(&path))? {
            return Self::check_existing(path, spec).await;
        }
        match spec {
            WorkspaceSpec::EmptyDir => create_dir(&path).await?,
            WorkspaceSpec::GitClone { repo, depth } => {
                let repo_text = repo
                    .to_str()
                    .ok_or_else(|| WorkspaceError::NonUtf8Repo { repo: repo.clone() })?;
                let check = self
                    .git(
                        &argv(&["git", "-C", repo_text, "rev-parse", "--git-dir"]),
                        repo,
                    )
                    .await?;
                if check.status != 0 {
                    return Err(WorkspaceError::NotAGitRepo { repo: repo.clone() });
                }
                create_dir(&self.root).await?;
                if let Err(e) = self.clone_repo(run, repo, repo_text, *depth, &text).await {
                    // Best effort: a half-made clone must not be mistaken for a workspace.
                    let _ = tokio::fs::remove_dir_all(&path).await;
                    return Err(e);
                }
            }
        }
        Ok(path)
    }

    /// Clones `repo` to `dest` and checks out the run branch. The checkout runs right after the
    /// clone only, when the clone's config and hooks are still ours.
    async fn clone_repo(
        &self,
        run: RunId,
        repo: &Path,
        repo_text: &str,
        depth: Option<u32>,
        dest: &str,
    ) -> Result<(), WorkspaceError> {
        let mut clone = argv(&["git", "clone", "--local", "--no-hardlinks"]);
        let source = match depth {
            Some(n) => {
                clone.extend(argv(&["--depth", &n.to_string()]));
                format!("file://{repo_text}")
            }
            None => repo_text.to_string(),
        };
        clone.extend(["--".to_string(), source, dest.to_string()]);
        let branch = Self::branch_for(run);
        let checkout = argv(&[
            "git",
            "-C",
            dest,
            "-c",
            "core.hooksPath=/dev/null",
            "checkout",
            "-b",
            &branch,
        ]);
        for command in [clone, checkout] {
            self.run_git(command, repo).await?;
        }
        Ok(())
    }

    /// An existing run directory is reused without running git, if it looks like a workspace.
    async fn check_existing(
        path: PathBuf,
        spec: &WorkspaceSpec,
    ) -> Result<PathBuf, WorkspaceError> {
        let bad = |path: PathBuf, reason: &str| WorkspaceError::BadExisting {
            path,
            reason: reason.to_string(),
        };
        if !tokio::fs::metadata(&path)
            .await
            .map_err(io(&path))?
            .is_dir()
        {
            return Err(bad(path, "not a directory"));
        }
        let has_git = tokio::fs::try_exists(path.join(".git"))
            .await
            .map_err(io(&path))?;
        if matches!(spec, WorkspaceSpec::GitClone { .. }) && !has_git {
            return Err(bad(path, "no `.git` entry"));
        }
        Ok(path)
    }

    /// Brings the run's branch home: runs ONLY from the source `repo` (never inside the clone,
    /// whose config and hooks the agent controls) and creates `kestrel/run-<id tail>` there.
    pub async fn fetch_back(&self, run: RunId, repo: &Path) -> Result<(), WorkspaceError> {
        let repo_text = repo.to_str().ok_or_else(|| WorkspaceError::NonUtf8Repo {
            repo: repo.to_path_buf(),
        })?;
        let branch = Self::branch_for(run);
        let clone_dir = self.path_for(run);
        // The clone is the agent's: refuse anything that could point git at another repo.
        // A clone directory that does not exist at all is left for git to report.
        if let Ok(meta) = tokio::fs::symlink_metadata(&clone_dir).await {
            let objects_info = clone_dir.join(".git/objects/info");
            let git_dir = tokio::fs::symlink_metadata(clone_dir.join(".git")).await;
            if !meta.is_dir() || !git_dir.is_ok_and(|m| m.is_dir()) {
                return Err(WorkspaceError::GitDirNotADirectory { clone: clone_dir });
            }
            for name in ["alternates", "http-alternates"] {
                if tokio::fs::symlink_metadata(objects_info.join(name))
                    .await
                    .is_ok()
                {
                    return Err(WorkspaceError::AlternatesPresent { clone: clone_dir });
                }
            }
        }
        let clone = clone_dir.to_string_lossy().into_owned();
        // No `+` on the refspec: a branch that cannot fast-forward is an error on purpose.
        let command = argv(&[
            "git",
            "-C",
            repo_text,
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "protocol.ext.allow=never",
            "fetch",
            "--no-tags",
            "--",
            &clone,
            &format!("{branch}:{branch}"),
        ]);
        self.run_git(command, repo).await
    }

    /// The commit `run`'s workspace is at, a full commit id.
    ///
    /// Only for a workspace [`Workspaces::prepare`] has just cloned, before its sandbox exists:
    /// git runs inside the clone, which is safe only while its config and hooks are still ours.
    /// Never call it on a workspace an agent has had.
    pub async fn head(&self, run: RunId) -> Result<String, WorkspaceError> {
        let path = self.path_for(run);
        let text = path.to_string_lossy().into_owned();
        let command = argv(&[
            "git",
            "-C",
            &text,
            "-c",
            "core.hooksPath=/dev/null",
            "rev-parse",
            "--verify",
            "HEAD^{commit}",
        ]);
        let out = run_git(self.runner.as_ref(), command, &path).await?;
        commit_id(out.trim())
    }

    /// Runs one git command and turns a non-zero exit into [`WorkspaceError::Git`].
    async fn run_git(&self, command: Vec<String>, repo: &Path) -> Result<(), WorkspaceError> {
        run_git(self.runner.as_ref(), command, repo)
            .await
            .map(|_| ())
    }

    /// Runs one git command; failing to start git is an `Io` error at `repo`.
    async fn git(
        &self,
        argv: &[String],
        repo: &Path,
    ) -> Result<kestrel_sandbox::CommandOutput, WorkspaceError> {
        git(self.runner.as_ref(), argv, repo).await
    }
}

/// Clones `repo` into `dest` and checks out `commit`, detached, the way a workspace is cloned
/// (`--local --no-hardlinks`, hooks off, the environment of [`git_env_scrub`]).
///
/// For copies the host keeps to itself, such as the one Travsr indexes for a run: `dest` must
/// not exist, and no agent ever gets it. `commit` must be a full commit id, so it can never be
/// read as an option. A copy that fails halfway is removed.
///
/// The copy holds no link: a link the commit holds is checked out as a plain file with its
/// target's path as text (`core.symlinks=false`), so a tool that writes files in the copy (such as
/// `travsr init`, which rewrites `.gitignore`) cannot write through one to the host. The clone
/// itself writes no working-tree file (`--no-checkout`); the checkout writes them all, and the
/// setting is also kept in the copy's config for any later git call there.
pub async fn clone_at(
    runner: &dyn CommandRunner,
    repo: &Path,
    commit: &str,
    dest: &Path,
) -> Result<(), WorkspaceError> {
    let commit = commit_id(commit)?;
    if repo.is_relative() {
        return Err(WorkspaceError::RelativeRepo {
            repo: repo.to_path_buf(),
        });
    }
    let repo_text = repo.to_str().ok_or_else(|| WorkspaceError::NonUtf8Repo {
        repo: repo.to_path_buf(),
    })?;
    let dest_text = dest.to_string_lossy();
    // Kept in the copy's own config, so it wins over the operator's global settings for every
    // later git call there: Travsr installs hooks wherever `core.hooksPath` points, and a global
    // one would put them in a hooks directory the operator shares with their other repos.
    let hooks = format!(
        "core.hooksPath={}",
        dest.join(".git").join("hooks").to_string_lossy()
    );
    let clone = argv(&[
        "git",
        "clone",
        "--local",
        "--no-hardlinks",
        "--no-checkout",
        "--config",
        "core.symlinks=false",
        "--config",
        &hooks,
        "--config",
        "core.fsmonitor=false",
        "--",
        repo_text,
        &dest_text,
    ]);
    let checkout = argv(&[
        "git",
        "-C",
        &dest_text,
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.symlinks=false",
        "checkout",
        "--detach",
        &commit,
    ]);
    for command in [clone, checkout] {
        if let Err(e) = run_git(runner, command, repo).await {
            // Best effort: a half-made copy must not be mistaken for a whole one.
            let _ = tokio::fs::remove_dir_all(dest).await;
            return Err(e);
        }
    }
    Ok(())
}

/// `text` when it is a full commit id: 40 (sha-1) or 64 (sha-256) lowercase hex characters.
fn commit_id(text: &str) -> Result<String, WorkspaceError> {
    let hex = text
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex && matches!(text.len(), 40 | 64) {
        return Ok(text.to_string());
    }
    let mut cut = text.len().min(80);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Err(WorkspaceError::BadCommit {
        text: text[..cut].to_string(),
    })
}

/// Runs one git command and returns its stdout; a non-zero exit is [`WorkspaceError::Git`].
async fn run_git(
    runner: &dyn CommandRunner,
    command: Vec<String>,
    repo: &Path,
) -> Result<String, WorkspaceError> {
    let out = git(runner, &command, repo).await?;
    if out.status != 0 {
        return Err(WorkspaceError::Git {
            argv: command,
            status: out.status,
            stderr: out.stderr.trim_end().to_string(),
        });
    }
    Ok(out.stdout)
}

/// Runs one git command with [`git_env_scrub`]; failing to start git is an `Io` error at `repo`.
async fn git(
    runner: &dyn CommandRunner,
    argv: &[String],
    repo: &Path,
) -> Result<kestrel_sandbox::CommandOutput, WorkspaceError> {
    runner
        .run_with_env(argv, &git_env_scrub())
        .await
        .map_err(io(repo))
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

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}
