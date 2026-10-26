//! `eidra eval vacuum-check`: tests the vacuum (docs/DESIGN.md §13) instead of the agent.
//!
//! The vacuum is not a setting the agent can turn off: it is the container's network namespace,
//! and the only thing in it is the mounted socket the model answers on. That claim is worth a
//! test, so this command runs `vacuum_probe.py` in the very container an attempt's agent gets —
//! the same builder, the same relay — and asks it to try every way out we know of. The probe
//! exits non-zero if any of them worked.
//!
//! Both shapes of vacuum are checked, since they close the network differently:
//! - `none`: the agent's own container, started with `--network none` (eval/mod.rs).
//! - `disconnected`: a Harbor task's container, started on the bridge and disconnected before the
//!   agent runs (eval/harbor.rs), where a disconnect that silently failed would be invisible.
//!
//! And `control` runs the probe on the host's network, where it must report a leak. A probe that
//! passes everywhere tests nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Args;
use eidra_core::Profile;

use super::docker::Container;
use super::{Extras, agent_container, harbor};
use crate::relay;

const PROBE: &str = include_str!("vacuum_probe.py");
/// Long enough for the probe's connects to time out several times over.
const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Args)]
pub struct VacuumArgs {
    /// The image the probe runs in; it needs python3, as the suite's tasks do
    #[arg(long, default_value = "python:3.13-slim")]
    image: String,

    /// A profile directory, for the model server to relay to [default: the built-in profile]
    #[arg(long, value_name = "DIR")]
    profile: Option<PathBuf>,

    /// Which vacuum to check [default: every one, and the control]
    #[arg(long, value_enum)]
    mode: Option<Mode>,

    /// Where the probe's logs go [default: a directory under the temporary directory]
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Mode {
    /// The agent's container for a suite task: `--network none`
    None,
    /// A Harbor task's container: on the bridge, then disconnected
    Disconnected,
    /// The probe on the host's network, where it must find a way out
    Control,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::None => "none",
            Mode::Disconnected => "disconnected",
            Mode::Control => "control",
        }
    }
}

pub async fn run(args: VacuumArgs) -> Result<i32> {
    let (profile, profile_dir) = match &args.profile {
        Some(dir) => (Profile::load(dir)?, Some(fs::canonicalize(dir)?)),
        None => (Profile::builtin()?, None),
    };
    let target = relay::target_of(&profile.base_url)?;
    let out = match &args.out {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir().join(format!("eidra-vacuum-{}", std::process::id())),
    };
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let out = fs::canonicalize(&out)?;
    let probe_dir = out.join("probe");
    fs::create_dir_all(&probe_dir)?;
    fs::write(probe_dir.join("probe.py"), PROBE)?;

    // The relay the model answers on, as a run's agents get it.
    let socket = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join(format!("eidra-vacuum-{}.sock", std::process::id()));
    let listener = relay::bind(&socket).context("starting the model relay")?;
    let relay = tokio::spawn(relay::forward(listener, target.clone()));

    let binary = std::env::current_exe().context("finding the eidra binary")?;
    let setup = Setup {
        image: &args.image,
        binary: &binary,
        socket: &socket,
        target: &target,
        api_key_env: profile.api_key_env.as_deref(),
        profile_dir: profile_dir.as_deref(),
        probe_dir: &probe_dir,
    };

    eprintln!("vacuum-check · image {} · relaying {} to {target}", args.image, socket.display());
    let modes = match args.mode {
        Some(mode) => vec![mode],
        None => vec![Mode::None, Mode::Disconnected, Mode::Control],
    };
    let mut problems = 0;
    for mode in modes {
        let dir = out.join(mode.name());
        fs::create_dir_all(&dir)?;
        let log = dir.join("probe.log");
        let outcome = match mode {
            Mode::None => no_network(&setup, &dir, &log).await,
            Mode::Disconnected => disconnected(&setup, &dir, &log).await,
            Mode::Control => host_network(&setup, &dir, &log).await,
        };
        let verdict = match outcome {
            // The probe exits 0 when nothing got out, which is what a vacuum must do and what the
            // control must not.
            Ok(code) => {
                let sealed = code == Some(0);
                let ok = sealed == (mode != Mode::Control);
                println!(
                    "{:<14} {:<48} {}",
                    mode.name(),
                    summary(&log, code),
                    if ok { "ok" } else { "PROBLEM" }
                );
                ok
            }
            Err(error) => {
                println!("{:<14} PROBLEM: {error:#}", mode.name());
                false
            }
        };
        if !verdict {
            problems += 1;
            print_log(&log);
            println!("  probe log: {}", log.display());
        }
    }
    relay.abort();
    let _ = fs::remove_file(&socket);
    if problems == 0 {
        println!("\nthe vacuum holds: nothing but the model is reachable from inside it");
        if args.out.is_none() {
            let _ = fs::remove_dir_all(&out);
        } else {
            println!("probe logs are under {}", out.display());
        }
    } else {
        println!("\n{problems} of the checks did not hold; logs are under {}", out.display());
    }
    Ok(i32::from(problems > 0))
}

/// What every mode needs to start a container the probe can run in.
struct Setup<'a> {
    image: &'a str,
    binary: &'a Path,
    socket: &'a Path,
    target: &'a str,
    api_key_env: Option<&'a str>,
    profile_dir: Option<&'a Path>,
    probe_dir: &'a Path,
}

/// The agent's container for a suite task, built by the same code an attempt uses, running the
/// probe instead of the agent.
async fn no_network(setup: &Setup<'_>, dir: &Path, log: &Path) -> Result<Option<i32>> {
    let (work, sessions) = (dir.join("work"), dir.join("sessions"));
    fs::create_dir_all(&work)?;
    fs::create_dir_all(&sessions)?;
    let container = agent_container(
        setup.image,
        setup.binary,
        &work,
        &sessions,
        &Extras {
            model_socket: Some(setup.socket),
            profile_dir: setup.profile_dir,
            api_key_env: setup.api_key_env,
            tools: None,
        },
    )
    .mount(setup.probe_dir, "/probe", true)
    .set_env("EIDRA_PROBE_TARGET", setup.target)
    .set_env("EIDRA_PROBE_JSON", "/work/probe.json");
    let finished = container.run(&["python3", "/probe/probe.py"], TIMEOUT, log, None).await?;
    Ok(if finished.timed_out { None } else { finished.code })
}

/// The control: the same probe with the host's network, where it has to find a way out. The socket
/// is mounted too, so that only the checks about reaching the network can fail.
async fn host_network(setup: &Setup<'_>, dir: &Path, log: &Path) -> Result<Option<i32>> {
    let work = dir.join("work");
    fs::create_dir_all(&work)?;
    let container = Container::new(setup.image, "host")
        .mount(setup.binary, "/usr/local/bin/eidra", true)
        .mount(&work, "/work", false)
        .mount(setup.socket, harbor::SOCKET_IN_CONTAINER, false)
        .mount(setup.probe_dir, "/probe", true)
        .set_env("EIDRA_MODEL_SOCKET", harbor::SOCKET_IN_CONTAINER)
        .set_env("EIDRA_PROBE_TARGET", setup.target);
    let finished = container.run(&["python3", "/probe/probe.py"], TIMEOUT, log, None).await?;
    Ok(if finished.timed_out { None } else { finished.code })
}

/// A Harbor task's container: started on the bridge with the agent's mounts and disconnected
/// before anything of the agent's runs, as harbor.rs does it.
async fn disconnected(setup: &Setup<'_>, dir: &Path, log: &Path) -> Result<Option<i32>> {
    let sessions = dir.join("sessions");
    fs::create_dir_all(&sessions)?;
    let config = harbor::Config::minimal();
    let route = harbor::Route::Socket(setup.socket.to_path_buf());
    let agent = harbor::AgentSetup {
        route: &route,
        binary: setup.binary,
        sessions: &sessions,
        profile_dir: setup.profile_dir,
        api_key_env: setup.api_key_env,
        tools: None,
    };
    let mut env = harbor::Env::start(setup.image, &config, Some(&agent)).await?;
    let outcome = probe_in(&mut env, setup, log).await;
    let image = env.image().to_owned();
    env.remove().await;
    harbor::give_back(&sessions, &image).await;
    outcome
}

async fn probe_in(env: &mut harbor::Env, setup: &Setup<'_>, log: &Path) -> Result<Option<i32>> {
    env.set_network(false).await?;
    env.copy_in(setup.probe_dir, "/probe").await?;
    let mut vars = vec![format!("EIDRA_PROBE_TARGET={}", setup.target)];
    vars.extend(setup.api_key_env.map(str::to_owned));
    let finished =
        env.exec(&["python3", "/probe/probe.py"], None, &vars, TIMEOUT, log, None).await?;
    Ok(if finished.timed_out { None } else { finished.code })
}

/// The probe's own summary line, for the report.
fn summary(log: &Path, code: Option<i32>) -> String {
    let text = fs::read_to_string(log).unwrap_or_default();
    match text.lines().rev().find(|line| line.starts_with("summary: ")) {
        Some(line) => line.trim_start_matches("summary: ").to_owned(),
        None => match code {
            Some(code) => format!("the probe said nothing and exited {code}"),
            None => "the probe ran out of time".to_owned(),
        },
    }
}

fn print_log(log: &Path) {
    if let Ok(text) = fs::read_to_string(log) {
        for line in text.lines() {
            println!("  {line}");
        }
    }
}
