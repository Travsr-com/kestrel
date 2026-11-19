//! The `kestrel` command-line tool.
//!
//! M1 has no HTTP API. Every command except `serve` talks to the engine only through the SQLite
//! store (`--db`): `run` writes a `Queued` run, the steering commands push control commands, and
//! `watch` and `ps` read runs and events. `kestrel serve` runs the engine and the LLM gateway in one
//! process on the same database.
//!
//! Exit codes: [`EXIT_OK`], [`EXIT_RUNTIME`], [`EXIT_USAGE`].

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use kestrel_types::{ApprovalId, RunId};

/// Success.
pub const EXIT_OK: u8 = 0;
/// Runtime error: unknown run, store failure, engine failure.
pub const EXIT_RUNTIME: u8 = 1;
/// Bad usage or a bad agent or config file. Clap also exits with 2 on a usage error.
pub const EXIT_USAGE: u8 = 2;

/// Run and watch fleets of AI agents.
#[derive(Debug, Parser)]
#[command(name = "kestrel", version, about, arg_required_else_help = true)]
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
    /// Submit a run and print its id.
    Run(RunArgs),
    /// Follow a run's events live.
    Watch { run_id: RunId },
    /// List active runs.
    Ps {
        /// List finished runs too.
        #[arg(long)]
        all: bool,
    },
    /// Resolve a pending approval.
    Approve {
        approval_id: ApprovalId,
        /// Reject instead of approve.
        #[arg(long)]
        reject: bool,
    },
    /// Pause a run.
    Pause { run_id: RunId },
    /// Resume a paused or suspended run.
    Resume { run_id: RunId },
    /// Cancel a run.
    Cancel { run_id: RunId },
    /// Set a run's budget in USD.
    Budget {
        run_id: RunId,
        /// New limit in USD, for example 5 or 2.50.
        #[arg(allow_negative_numbers = true)]
        usd: String,
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

/// Runs one parsed command and returns its exit code.
pub fn dispatch(cli: Cli) -> u8 {
    match cli.command {
        Command::Serve(_) => todo!("kestrel serve"),
        Command::Run(_) => todo!("kestrel run"),
        Command::Watch { .. } => todo!("kestrel watch"),
        Command::Ps { .. } => todo!("kestrel ps"),
        Command::Approve { .. } => todo!("kestrel approve"),
        Command::Pause { .. } => todo!("kestrel pause"),
        Command::Resume { .. } => todo!("kestrel resume"),
        Command::Cancel { .. } => todo!("kestrel cancel"),
        Command::Budget { .. } => todo!("kestrel budget"),
    }
}
