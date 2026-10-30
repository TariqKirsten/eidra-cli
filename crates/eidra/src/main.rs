//! eidra: a coding agent tuned for one local model. See docs/DESIGN.md.

mod eval;
mod exec;
mod format;
mod interactive;
mod probe;
mod relay;
mod replay;
mod sessions;

use clap::{Parser, Subcommand};

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
    /// Forward to the model server: a Unix socket, for an agent in a container with no network, or
    /// TLS on a port (--listen), for eidra on another machine
    Relay(relay::RelayArgs),
    /// List this directory's sessions, to go on with one with --continue or --resume
    Sessions(sessions::SessionsArgs),
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
    }
}
