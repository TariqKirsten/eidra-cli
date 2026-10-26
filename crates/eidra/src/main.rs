//! eidra: a coding agent tuned for one local model. See docs/DESIGN.md.

mod eval;
mod exec;
mod format;
mod probe;
mod replay;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "eidra", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
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
    }
}
