//! The `kestrel` command-line tool.
//!
//! M1 has no HTTP API. Every command except `serve` talks to the engine only through the SQLite
//! store (`--db`): `run` writes a `Queued` run, the steering commands push control commands, and
//! `watch` and `ps` read runs and events. `kestrel serve` runs the engine and the LLM gateway in one
//! process on the same database. `fetch` reads a finished run from the store and runs git in its
//! source repo to bring its branch home, as the engine does when a run ends.
//!
//! Exit codes: [`EXIT_OK`], [`EXIT_RUNTIME`], [`EXIT_USAGE`].

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use kestrel_store::{ControlCommand, Store, StoreError};
use kestrel_types::{ApprovalId, MicroUsd, RunId};

mod commands;
#[cfg(debug_assertions)]
mod fake;
mod fetch;
mod paths;
mod serve;
mod tools;
mod travsr;

/// Success.
pub const EXIT_OK: u8 = 0;
/// Runtime error: unknown run, store failure, engine failure.
pub const EXIT_RUNTIME: u8 = 1;
/// Bad usage or a bad agent or config file. Clap also exits with 2 on a usage error.
pub const EXIT_USAGE: u8 = 2;

/// Run and watch fleets of AI agents.
#[derive(Debug, Parser)]
#[command(
    name = "kestrel",
    version,
    about = "Run and watch fleets of AI agents.",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Database file. Defaults to `kestrel.db` under the user data dir.
    #[arg(long, global = true, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the engine and the gateway in the foreground.
    Serve(ServeArgs),
    /// Run the engine, the gateway and the API server with the web UI, and open the UI.
    Up(UpArgs),
    /// Submit a run and print its id.
    Run(RunArgs),
    /// Follow a run's events live.
    Watch {
        /// Run id, as printed by `kestrel run`.
        run_id: RunId,
    },
    /// List active runs.
    Ps {
        /// List finished runs too.
        #[arg(long)]
        all: bool,
    },
    /// Resolve a pending approval.
    Approve {
        /// Approval id, as shown by `kestrel watch`.
        approval_id: ApprovalId,
        /// Reject instead of approve.
        #[arg(long)]
        reject: bool,
    },
    /// Pause a run.
    Pause {
        /// Run id, as printed by `kestrel run`.
        run_id: RunId,
    },
    /// Resume a paused or suspended run.
    Resume {
        /// Run id, as printed by `kestrel run`.
        run_id: RunId,
    },
    /// Cancel a run.
    Cancel {
        /// Run id, as printed by `kestrel run`.
        run_id: RunId,
    },
    /// Set a run's budget in USD.
    Budget {
        /// Run id, as printed by `kestrel run`.
        run_id: RunId,
        /// New limit in USD, for example 5 or 2.50.
        #[arg(allow_negative_numbers = true)]
        usd: String,
    },
    /// Bring a finished run's branch into its source repo.
    Fetch {
        /// Run id, as printed by `kestrel run`.
        run_id: RunId,
    },
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Config file. Defaults to `~/.config/kestrel/config.toml`.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Test only: use a fake sandbox driver. Exists in debug builds only, so release binaries
    /// never ship it.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true, value_name = "NAME")]
    pub sandbox_driver: Option<String>,
}

#[derive(Debug, Args)]
pub struct UpArgs {
    #[command(flatten)]
    pub serve: ServeArgs,

    /// Where the API server and the UI listen. Defaults to 127.0.0.1:7171. Another address
    /// works too, with a warning: the token is then all that protects it.
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<String>,

    /// Print the UI address but do not open a browser.
    #[arg(long)]
    pub no_open: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Agent spec file (YAML).
    pub agent: PathBuf,
    /// The task for the agent.
    #[arg(long)]
    pub input: Option<String>,
    /// Follow the run after submitting it.
    #[arg(long)]
    pub watch: bool,
}

/// Entry point of the `kestrel` binary.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    ExitCode::from(dispatch(cli))
}

/// Runs one parsed command and returns its exit code. Errors go to stderr.
pub fn dispatch(cli: Cli) -> u8 {
    match execute(cli) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("error: {}", e.message);
            e.code
        }
    }
}

fn execute(cli: Cli) -> Result<(), Failure> {
    let out = Output { json: cli.json };
    let db = cli.db;
    match cli.command {
        Command::Serve(args) => serve::serve(db, args, out),
        Command::Up(args) => serve::up(db, args, out),
        Command::Run(args) => commands::run(db, args, out),
        Command::Watch { run_id } => commands::watch(&open(db)?, run_id, out),
        Command::Ps { all } => commands::ps(&open(db)?, all, out),
        Command::Approve {
            approval_id,
            reject,
        } => commands::approve(&open(db)?, approval_id, reject, out),
        Command::Pause { run_id } => {
            commands::steer(&open(db)?, run_id, ControlCommand::Pause, out)
        }
        Command::Resume { run_id } => {
            commands::steer(&open(db)?, run_id, ControlCommand::Resume, out)
        }
        Command::Cancel { run_id } => {
            commands::steer(&open(db)?, run_id, ControlCommand::Cancel, out)
        }
        Command::Budget { run_id, usd } => {
            let limit = MicroUsd::parse_decimal(&usd).ok_or_else(|| {
                Failure::usage(format!(
                    "`{usd}` is not a dollar amount (use a non-negative number like 5 or 2.50)"
                ))
            })?;
            commands::steer(&open(db)?, run_id, ControlCommand::SetBudget(limit), out)
        }
        Command::Fetch { run_id } => fetch::fetch(db, run_id, out),
    }
}

/// A failed command: what to print and which exit code to use.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) code: u8,
    pub(crate) message: String,
}

impl Failure {
    pub(crate) fn usage(message: impl Into<String>) -> Failure {
        Failure {
            code: EXIT_USAGE,
            message: message.into(),
        }
    }

    pub(crate) fn runtime(message: impl Into<String>) -> Failure {
        Failure {
            code: EXIT_RUNTIME,
            message: message.into(),
        }
    }
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Failure {
        Failure::runtime(e.to_string())
    }
}

/// How to print results.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Output {
    pub(crate) json: bool,
}

/// Opens the store at `db`, or at the default path under the user data dir.
pub(crate) fn open(db: Option<PathBuf>) -> Result<Store, Failure> {
    let path = db_path(db)?;
    Store::open(&path).map_err(|e| Failure::runtime(format!("{}: {e}", path.display())))
}

/// The directory the database is in, absolute. Workspaces and run configs live next to it, so a
/// temp `--db` keeps everything in one place.
pub(crate) fn data_dir_of(db: &Path) -> Result<PathBuf, Failure> {
    std::path::absolute(db)
        .map_err(|e| Failure::runtime(format!("{}: {e}", db.display())))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Failure::runtime(format!("{}: no parent dir", db.display())))
}

/// Where `kestrel serve` keeps the runs' workspaces: `<data dir>/workspaces`.
pub(crate) fn workspace_root(data_dir: &Path) -> PathBuf {
    data_dir.join("workspaces")
}

/// `db`, or `kestrel.db` under the user data dir (created if needed).
pub(crate) fn db_path(db: Option<PathBuf>) -> Result<PathBuf, Failure> {
    match db {
        Some(p) => Ok(p),
        None => {
            let dir = paths::data_dir()?.join("kestrel");
            std::fs::create_dir_all(&dir)
                .map_err(|e| Failure::runtime(format!("{}: {e}", dir.display())))?;
            Ok(dir.join("kestrel.db"))
        }
    }
}
