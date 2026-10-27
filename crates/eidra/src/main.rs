//! eidra: a coding agent tuned for one local model. See docs/DESIGN.md.

mod eval;
mod exec;
mod format;
mod interactive;
mod probe;
mod relay;
mod replay;
mod sessions;
mod vault;

use std::io;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use eidra_core::snapshot_filter;

#[derive(Debug, Parser)]
#[command(
    name = "eidra",
    version,
    about,
    args_conflicts_with_subcommands = true,
    after_help = "Without a command, eidra starts an interactive session in the terminal."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    agent: exec::AgentArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Do one task unattended, in the current directory
    Exec(exec::ExecArgs),
    /// Run a task suite against the agent, and report and compare runs
    Eval(eval::EvalArgs),
    /// Measure how much of each prompt the server's prefix cache reuses
    Probe(probe::ProbeArgs),
    /// Send one request of a recorded session again, with its recorded seed or new ones
    Replay(replay::ReplayArgs),
    /// Forward a Unix socket to the model server, for an agent in a container with no network
    Relay(relay::RelayArgs),
    /// List this directory's sessions, to go on with one with --continue or --resume
    Sessions(sessions::SessionsArgs),
    /// Print your own AGENTS.md from ~/.eidra, decrypted when ~/.eidra is encrypted
    Instructions,
    /// git's filter for encrypted snapshots, which git runs (docs/DESIGN.md §18)
    #[command(hide = true)]
    SnapshotFilter {
        /// process: git's long-running filter protocol; textconv FILE: a snapshotted file, opened
        mode: String,
        file: Option<PathBuf>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return interactive::run(cli.agent).await;
    };
    match command {
        Command::Exec(args) => {
            // The agent, and the background jobs it started, are gone once run returns.
            let code = exec::run(args).await?;
            std::process::exit(code)
        }
        Command::Eval(args) => {
            let code = eval::run(args).await?;
            std::process::exit(code)
        }
        Command::Probe(args) => probe::run(args).await,
        Command::Replay(args) => replay::run(args).await,
        Command::Relay(args) => relay::run(args).await,
        Command::Sessions(args) => sessions::run(args),
        Command::Instructions => instructions(),
        Command::SnapshotFilter { mode, file } => {
            let cipher = snapshot_filter::cipher_from_env()?;
            match (mode.as_str(), file) {
                ("process", None) => snapshot_filter::serve(&cipher, io::stdin(), io::stdout())?,
                ("textconv", Some(file)) => {
                    snapshot_filter::textconv(&cipher, &file, io::stdout())?
                }
                _ => bail!("eidra snapshot-filter takes process, or textconv and a file"),
            }
            Ok(())
        }
    }
}

/// Prints the user's own AGENTS.md: a password first, if eidra's directory is encrypted.
fn instructions() -> anyhow::Result<()> {
    let here = std::env::current_dir().context("the current directory")?;
    let home = eidra_core::eidra_home(&here);
    let text = match vault::open(&home, false)? {
        Some(vault) => vault.instructions(&home)?,
        None => std::fs::read_to_string(home.join("AGENTS.md")).ok(),
    };
    match text {
        Some(text) => print!("{text}"),
        None => eprintln!(
            "there is no AGENTS.md in {}: put one there, and the next start encrypts it",
            home.display()
        ),
    }
    Ok(())
}
