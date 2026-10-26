//! Harbor-format tasks (docs/DESIGN.md §15, https://docs.harborframework.com/tasks/overview):
//! - instruction.md: the prompt
//! - task.toml: timeouts, resources, network and environment variables
//! - environment/: a Dockerfile, or task.toml names a prebuilt image
//! - tests/test.sh: the verifier, which writes a reward to /logs/verifier/reward.txt (or .json)
//! - solution/solve.sh: the reference solution, used only by `eidra eval validate`
//!
//! The agent and then the verifier run in one container of the task's image, as Harbor runs tasks
//! that do not ask for a separate verifier: the verifier sees what the agent left. An attempt
//! passes when the reward is at least 1. Multi-step tasks, separate verifiers and docker-compose
//! environments are not supported.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use eidra_core::fnv1a;
use serde::Deserialize;
use tokio::process::Command;

use super::docker::{Finished, user};
use super::task::Task;
use super::{Attempt, CheckResult, RunContext, metrics};

/// Where the model socket is mounted in the container, and `EIDRA_MODEL_SOCKET` points.
pub const SOCKET_IN_CONTAINER: &str = "/run/eidra/model.sock";
/// Requests an agent may make on a Harbor task, unless the run says otherwise. These tasks are
/// bounded by their agent timeout (SWE-rebench: 50 minutes), so this only stops a loop.
pub const MAX_REQUESTS: usize = 200;
/// The agent's time limit when task.toml sets none (Harbor's default is no limit).
const AGENT_TIMEOUT_S: f64 = 3600.0;

static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

/// How the agent reaches the model server.
#[derive(Debug, Clone)]
pub enum Route {
    /// The container shares the host's network, so the agent and everything it runs reach the
    /// server and the internet.
    Host,
    /// The container has no network but loopback while the agent runs; model requests go through
    /// a Unix socket on the host (the model relay), mounted at [`SOCKET_IN_CONTAINER`].
    Socket(PathBuf),
}

/// What eidra needs from a task's task.toml.
#[derive(Debug, Clone)]
pub struct Config {
    pub source: Option<String>,
    pub agent_timeout_s: f64,
    pub agent_user: Option<String>,
    pub verifier_timeout_s: f64,
    pub verifier_user: Option<String>,
    pub verifier_env: BTreeMap<String, String>,
    /// Whether the verifier needs the network, as `network_mode` (or the older `allow_internet`)
    /// says; Harbor's default is yes.
    pub verifier_network: bool,
    /// A prebuilt image; without one, environment/ is built.
    pub docker_image: Option<String>,
    pub build_timeout_s: f64,
    /// The working directory for the agent and the verifier; the image's own when unset.
    pub workdir: Option<String>,
    pub env: BTreeMap<String, String>,
    pub cpus: Option<f64>,
    pub memory_mb: Option<u64>,
    pub solution_env: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct File {
    source: Option<String>,
    steps: Option<toml::Value>,
    metadata: toml::Table,
    verifier: Verifier,
    agent: Agent,
    environment: Environment,
    solution: Solution,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Verifier {
    timeout_sec: Option<f64>,
    user: Option<toml::Value>,
    env: BTreeMap<String, String>,
    network_mode: Option<String>,
    environment_mode: Option<String>,
    environment: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Agent {
    timeout_sec: Option<f64>,
    user: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Environment {
    build_timeout_sec: Option<f64>,
    docker_image: Option<String>,
    network_mode: Option<String>,
    allow_internet: Option<bool>,
    workdir: Option<String>,
    env: BTreeMap<String, String>,
    cpus: Option<f64>,
    memory_mb: Option<u64>,
    /// The older spelling: "4G", "512M".
    memory: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Solution {
    env: BTreeMap<String, String>,
}

impl Config {
    pub fn load(dir: &Path) -> Result<Config> {
        let path = dir.join("task.toml");
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let file: File = toml::from_str(&text).with_context(|| format!("{}", path.display()))?;
        if file.steps.is_some() || dir.join("steps").is_dir() {
            bail!("{}: multi-step Harbor tasks are not supported", dir.display());
        }
        if file.verifier.environment_mode.as_deref() == Some("separate")
            || file.verifier.environment.is_some()
        {
            bail!("{}: verifiers in a separate environment are not supported", dir.display());
        }
        if file.environment.docker_image.is_none() && !dir.join("environment/Dockerfile").is_file()
        {
            bail!("{}: neither environment/Dockerfile nor docker_image", dir.display());
        }
        let network = file.verifier.network_mode.or(file.environment.network_mode);
        let verifier_network = match network.as_deref() {
            Some("no-network") => false,
            Some(_) => true,
            None => file.environment.allow_internet.unwrap_or(true),
        };
        let memory_mb = file
            .environment
            .memory_mb
            .or_else(|| file.environment.memory.as_deref().and_then(megabytes));
        let source = file
            .source
            .or_else(|| file.metadata.get("source").and_then(|v| v.as_str()).map(str::to_owned));
        Ok(Config {
            source,
            agent_timeout_s: file.agent.timeout_sec.unwrap_or(AGENT_TIMEOUT_S),
            agent_user: file.agent.user.as_ref().map(user_name),
            verifier_timeout_s: file.verifier.timeout_sec.unwrap_or(600.0),
            verifier_user: file.verifier.user.as_ref().map(user_name),
            verifier_env: file.verifier.env,
            verifier_network,
            docker_image: file.environment.docker_image,
            build_timeout_s: file.environment.build_timeout_sec.unwrap_or(600.0),
            workdir: file.environment.workdir,
            env: file.environment.env,
            cpus: file.environment.cpus,
            memory_mb,
            solution_env: file.solution.env,
        })
    }
}

/// A user given as a name or a number.
fn user_name(value: &toml::Value) -> String {
    value.as_str().map_or_else(|| value.to_string(), str::to_owned)
}

/// "4G", "512M", "2048" (megabytes) as megabytes.
fn megabytes(size: &str) -> Option<u64> {
    let size = size.trim();
    let (number, unit) =
        size.split_at(size.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(size.len()));
    let number: f64 = number.trim().parse().ok()?;
    let factor = match unit.trim().to_ascii_uppercase().trim_end_matches('B') {
        "" | "M" => 1.0,
        "G" => 1024.0,
        "K" => 1.0 / 1024.0,
        "T" => 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor).round() as u64)
}

/// `${VAR}` and `${VAR:-default}` in task.toml's environment values, from this process's
/// environment, as Harbor reads them.
fn expand(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &rest[start + 2..start + end];
        let (name, default) = inner.split_once(":-").unwrap_or((inner, ""));
        out.push_str(&std::env::var(name).unwrap_or_else(|_| default.to_owned()));
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// The images a task's environment uses, and which of them this run fetched or made.
#[derive(Debug, Clone)]
pub struct Images {
    pub tag: String,
    /// Images to remove when the task is done: the built tag, and a base image this run pulled.
    pub created: Vec<String>,
}

/// The task's image: its prebuilt image, pulled if missing, or environment/ built and tagged by
/// its contents. Docker's output goes to `log`.
pub async fn image(id: &str, dir: &Path, config: &Config, log: &Path) -> Result<Images> {
    let timeout = Duration::from_secs_f64(config.build_timeout_s);
    if let Some(image) = &config.docker_image {
        if present(image).await {
            return Ok(Images { tag: image.clone(), created: Vec::new() });
        }
        let pulled = docker(&["pull", image], timeout, log).await?;
        if pulled.code != Some(0) {
            bail!("pulling {image} failed; see {}", log.display());
        }
        return Ok(Images { tag: image.clone(), created: vec![image.clone()] });
    }

    let context = dir.join("environment");
    let tag = format!("eidra-harbor/{}:{:016x}", image_name(id), tree_hash(&context)?);
    if present(&tag).await {
        return Ok(Images { tag: tag.clone(), created: vec![tag] });
    }
    let mut created = vec![tag.clone()];
    if let Some(base) = base_image(&context.join("Dockerfile"))
        && !present(&base).await
    {
        created.push(base);
    }
    let context = context.to_string_lossy();
    let built = docker(&["build", "-t", &tag, &context], timeout, log).await?;
    if built.code != Some(0) {
        bail!("building {tag} failed; see {}", log.display());
    }
    Ok(Images { tag, created })
}

/// Removes the images a task's attempts no longer need.
pub async fn remove_images(images: &Images) {
    for image in &images.created {
        let _ = quiet(&["rmi", "-f", image]).await;
    }
}

/// A lowercase name Docker accepts for a repository.
fn image_name(id: &str) -> String {
    id.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
        .collect()
}

/// The first `FROM` image of a Dockerfile, unless it is built from an argument.
fn base_image(dockerfile: &Path) -> Option<String> {
    let text = fs::read_to_string(dockerfile).ok()?;
    let line = text.lines().map(str::trim).find(|l| l.to_ascii_uppercase().starts_with("FROM "))?;
    let image = line.split_whitespace().skip(1).find(|word| !word.starts_with("--"))?.to_owned();
    (!image.contains('$') && image != "scratch").then_some(image)
}

/// A hash of every file under `dir`, by path and contents.
fn tree_hash(dir: &Path) -> Result<u64> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort();
    let mut summary = String::new();
    for path in files {
        let bytes = fs::read(dir.join(&path))?;
        summary.push_str(&format!("{path}\0{:016x}\n", fnv1a(&bytes)));
    }
    Ok(fnv1a(summary.as_bytes()))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect(root, &path, out)?;
        } else {
            out.push(path.strip_prefix(root)?.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

async fn present(image: &str) -> bool {
    quiet(&["image", "inspect", image]).await.is_ok_and(|code| code == 0)
}

/// What an attempt mounts and passes to the agent.
pub struct AgentSetup<'a> {
    pub route: &'a Route,
    pub binary: &'a Path,
    pub sessions: &'a Path,
    pub profile_dir: Option<&'a Path>,
    pub api_key_env: Option<&'a str>,
}

/// A running container of a task's image, in which the agent and then the verifier run.
pub struct Env {
    name: String,
    image: String,
    workdir: String,
    /// The `docker run` arguments, kept to start a replacement from a snapshot.
    run_args: Vec<String>,
    /// Snapshots taken of this environment, removed with it.
    snapshots: Vec<String>,
    /// Whether the container is on Docker's bridge network (else the host's), and connected now.
    bridge: bool,
    on_bridge: bool,
    /// Whether the container was stopped after a command ran out of time.
    stopped: bool,
}

impl Env {
    /// Starts a container of `image` that only waits, with the agent's mounts when `agent` is set.
    pub async fn start(
        image: &str,
        config: &Config,
        agent: Option<&AgentSetup<'_>>,
    ) -> Result<Env> {
        let name = format!(
            "eidra-harbor-{}-{}",
            std::process::id(),
            NEXT_NAME.fetch_add(1, Ordering::Relaxed)
        );
        let socket = match agent.map(|a| a.route) {
            Some(Route::Socket(socket)) => Some(socket.clone()),
            _ => None,
        };
        // The agent on the host's network reaches the server directly. Behind the relay it starts
        // on the bridge, which is disconnected while the agent runs.
        let network = if socket.is_some() || agent.is_none() { "bridge" } else { "host" };
        let mut args: Vec<String> =
            ["run", "-d", "--init", "--network", network].map(String::from).to_vec();
        if let Some(cpus) = config.cpus {
            args.extend(["--cpus".into(), cpus.to_string()]);
        }
        if let Some(mb) = config.memory_mb {
            args.extend(["--memory".into(), format!("{mb}m")]);
        }
        for (key, value) in &config.env {
            args.extend(["-e".into(), format!("{key}={}", expand(value))]);
        }
        if let Some(agent) = agent {
            let mut mount = |source: &Path, target: &str, mode: &str| {
                let source = std::path::absolute(source).unwrap_or_else(|_| source.to_path_buf());
                args.extend(["-v".into(), format!("{}:{target}{mode}", source.display())]);
            };
            mount(agent.binary, "/usr/local/bin/eidra", ":ro");
            mount(agent.sessions, "/sessions", "");
            if let Some(profile) = agent.profile_dir {
                mount(profile, "/profile", ":ro");
            }
            if let Some(socket) = &socket {
                mount(socket, SOCKET_IN_CONTAINER, "");
                args.extend(["-e".into(), format!("EIDRA_MODEL_SOCKET={SOCKET_IN_CONTAINER}")]);
            }
        }
        args.extend(["--entrypoint".into(), "tail".into()]);
        let workdir = match &config.workdir {
            Some(dir) => dir.clone(),
            None => {
                let (_, dir) =
                    output(&["image", "inspect", "--format", "{{.Config.WorkingDir}}", image])
                        .await?;
                let dir = dir.trim();
                if dir.is_empty() { "/".to_owned() } else { dir.to_owned() }
            }
        };
        let mut env = Env {
            name,
            image: image.to_owned(),
            workdir,
            run_args: args,
            snapshots: Vec::new(),
            bridge: network == "bridge",
            on_bridge: network == "bridge",
            stopped: false,
        };
        env.launch(image).await?;
        Ok(env)
    }

    async fn launch(&mut self, image: &str) -> Result<()> {
        let mut args: Vec<&str> = self.run_args.iter().map(String::as_str).collect();
        args.extend(["--name", &self.name, image, "-f", "/dev/null"]);
        let (code, text) = output(&args).await?;
        if code != 0 {
            bail!("starting a container of {image}: {}", text.trim());
        }
        self.on_bridge = self.bridge;
        self.stopped = false;
        Ok(())
    }

    /// Takes the container off the network, leaving loopback, or puts it back. A container on the
    /// host's network keeps it.
    pub async fn set_network(&mut self, connected: bool) -> Result<()> {
        if !self.bridge || connected == self.on_bridge {
            return Ok(());
        }
        let verb = if connected { "connect" } else { "disconnect" };
        let (code, text) = output(&["network", verb, "bridge", &self.name]).await?;
        if code != 0 {
            bail!("docker network {verb} bridge {}: {}", self.name, text.trim());
        }
        self.on_bridge = connected;
        Ok(())
    }

    /// Runs `command` in the working directory. On a timeout the container is stopped, since what
    /// the command started cannot be stopped one process at a time from outside; see
    /// [`Env::resume`].
    pub async fn exec(
        &mut self,
        command: &[&str],
        user: Option<&str>,
        env: &[String],
        timeout: Duration,
        stdout: &Path,
        stderr: Option<&Path>,
    ) -> Result<Finished> {
        let mut args: Vec<&str> = vec!["exec", "-w", &self.workdir];
        if let Some(user) = user {
            args.extend(["-u", user]);
        }
        for var in env {
            args.extend(["-e", var]);
        }
        args.push(&self.name);
        args.extend(command);
        let finished = run_logged(&args, timeout, stdout, stderr).await?;
        if finished.timed_out {
            let _ = quiet(&["kill", &self.name]).await;
            self.stopped = true;
        }
        Ok(finished)
    }

    /// After a timeout stopped the container, starts a replacement from a snapshot of its files,
    /// so that the verifier still sees what the agent left.
    pub async fn resume(&mut self) -> Result<()> {
        if !self.stopped {
            return Ok(());
        }
        let snapshot = format!("eidra-harbor-snapshot/{}:latest", self.name);
        let (code, text) = output(&["commit", &self.name, &snapshot]).await?;
        let _ = quiet(&["rm", "-f", &self.name]).await;
        if code != 0 {
            bail!("snapshotting {}: {}", self.name, text.trim());
        }
        self.snapshots.push(snapshot.clone());
        self.launch(&snapshot).await
    }

    /// Copies a host directory's contents to `target` in the container.
    pub async fn copy_in(&self, from: &Path, target: &str) -> Result<()> {
        let (code, text) = output(&["exec", &self.name, "mkdir", "-p", target]).await?;
        if code != 0 {
            bail!("mkdir {target}: {}", text.trim());
        }
        let source = format!("{}/.", from.display());
        let (code, text) = output(&["cp", &source, &format!("{}:{target}", self.name)]).await?;
        if code != 0 {
            bail!("copying {} in: {}", from.display(), text.trim());
        }
        Ok(())
    }

    /// Copies a directory out of the container, if it is there.
    pub async fn copy_out(&self, source: &str, to: &Path) -> Result<bool> {
        let _ = fs::remove_dir_all(to);
        let (code, _) =
            output(&["cp", &format!("{}:{source}", self.name), &to.to_string_lossy()]).await?;
        Ok(code == 0)
    }

    /// What the agent changed in the working directory, as a patch, if it is a git repository:
    /// tracked files against HEAD, then new files, without touching the index.
    pub async fn agent_patch(&self) -> Result<String> {
        let script = "git rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0
git -c core.quotepath=off diff --no-color HEAD
git ls-files --others --exclude-standard -z | xargs -0 -r -n1 git diff --no-color --no-index /dev/null
exit 0";
        let (_, text) =
            output(&["exec", "-w", &self.workdir, &self.name, "sh", "-c", script]).await?;
        Ok(text)
    }

    /// Stops and removes the container, and the snapshots taken of it.
    pub async fn remove(self) {
        let _ = quiet(&["rm", "-f", &self.name]).await;
        for snapshot in &self.snapshots {
            let _ = quiet(&["rmi", "-f", snapshot]).await;
        }
    }

    pub fn image(&self) -> &str {
        &self.image
    }
}

/// The verifier's outcome.
pub struct Verdict {
    pub finished: Finished,
    pub reward: Option<f64>,
}

/// Copies tests/ to /tests and runs `bash /tests/test.sh` in the working directory, logging to
/// `dir/verifier.log` and copying /logs/verifier to `dir/verifier/`.
pub async fn verify(
    env: &mut Env,
    task_dir: &Path,
    config: &Config,
    dir: &Path,
) -> Result<Verdict> {
    fs::create_dir_all(dir)?;
    env.set_network(config.verifier_network).await?;
    env.copy_in(&task_dir.join("tests"), "/tests").await?;
    let vars: Vec<String> =
        config.verifier_env.iter().map(|(k, v)| format!("{k}={}", expand(v))).collect();
    let (code, text) = output(&["exec", &env.name, "mkdir", "-p", "/logs/verifier"]).await?;
    if code != 0 {
        bail!("mkdir /logs/verifier: {}", text.trim());
    }
    let timeout = Duration::from_secs_f64(config.verifier_timeout_s);
    let log = dir.join("verifier.log");
    let finished = env
        .exec(
            &["bash", "/tests/test.sh"],
            config.verifier_user.as_deref(),
            &vars,
            timeout,
            &log,
            None,
        )
        .await?;
    let logs = dir.join("verifier");
    env.copy_out("/logs/verifier", &logs).await?;
    let reward = read_reward(&logs);
    Ok(Verdict { finished, reward })
}

/// The reward in reward.json (its `reward` value, or the smallest of its values) or reward.txt.
fn read_reward(dir: &Path) -> Option<f64> {
    if let Ok(text) = fs::read_to_string(dir.join("reward.json"))
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
    {
        if let Some(reward) = value.get("reward").and_then(serde_json::Value::as_f64) {
            return Some(reward);
        }
        if let Some(map) = value.as_object() {
            return map.values().filter_map(serde_json::Value::as_f64).reduce(f64::min);
        }
    }
    fs::read_to_string(dir.join("reward.txt")).ok()?.trim().parse().ok()
}

/// Makes what containers wrote into `dir` the invoking user's, using a container of `image`.
pub async fn give_back(dir: &Path, image: &str) {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mount = format!("{}:/mnt", dir.display());
    let _ = quiet(&[
        "run",
        "--rm",
        "--network",
        "none",
        "-v",
        &mount,
        "--entrypoint",
        "chown",
        image,
        "-R",
        &user(),
        "/mnt",
    ])
    .await;
}

/// The run command for the agent: `eidra exec` with the run's settings.
pub fn agent_command(setup: &AgentSetup<'_>, max_requests: usize, prompt: &str) -> Vec<String> {
    let mut command: Vec<String> =
        ["eidra", "exec", "--json", "--session-dir", "/sessions", "--max-requests"]
            .map(String::from)
            .to_vec();
    command.push(max_requests.to_string());
    if setup.profile_dir.is_some() {
        command.extend(["--profile".into(), "/profile".into()]);
    }
    command.push(prompt.to_owned());
    command
}

/// One attempt at a Harbor task: the agent in a container of the task's image, then the verifier
/// in the same container. A task whose environment cannot be made counts as a failed attempt, with
/// the reason in error.log, so that one bad image does not stop a long run.
pub async fn run_attempt(
    context: &RunContext,
    task: &Task,
    config: &Config,
    repeat: u32,
    max_requests: usize,
) -> Result<Attempt> {
    let dir = context.run_dir.join(task.id()).join(repeat.to_string());
    let sessions = dir.join("sessions");
    fs::create_dir_all(&sessions)?;
    let started = Instant::now();
    let failed = |error: anyhow::Error| -> Result<Attempt> {
        fs::write(dir.join("error.log"), format!("{error:#}\n"))?;
        let setup = CheckResult {
            name: "setup".into(),
            passed: false,
            timed_out: false,
            seconds: started.elapsed().as_secs_f64(),
            reward: None,
        };
        Ok(Attempt {
            task: task.id().to_owned(),
            split: task.spec.split,
            repeat,
            passed: false,
            checks: vec![setup],
            exit_code: None,
            timed_out: false,
            wall_s: 0.0,
            metrics: metrics::read(&sessions).unwrap_or_default(),
        })
    };
    let images = match images_for(context, task, config).await {
        Ok(images) => images,
        Err(error) => return failed(error),
    };
    let setup = AgentSetup {
        route: &context.route,
        binary: &context.binary,
        sessions: &sessions,
        profile_dir: context.profile_dir.as_deref(),
        api_key_env: context.api_key_env.as_deref(),
    };
    let mut env = match Env::start(&images.tag, config, Some(&setup)).await {
        Ok(env) => env,
        Err(error) => return failed(error),
    };
    let outcome = agent_then_verifier(&mut env, &setup, task, config, max_requests, &dir).await;
    let image = env.image().to_owned();
    env.remove().await;
    give_back(&sessions, &image).await;
    let (finished, verdict) = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return failed(error),
    };
    let check = CheckResult {
        name: "verifier".into(),
        passed: !verdict.finished.timed_out && verdict.reward.is_some_and(|r| r >= 1.0),
        timed_out: verdict.finished.timed_out,
        seconds: verdict.finished.seconds,
        reward: verdict.reward,
    };
    Ok(Attempt {
        task: task.id().to_owned(),
        split: task.spec.split,
        repeat,
        passed: check.passed,
        checks: vec![check],
        exit_code: finished.code,
        timed_out: finished.timed_out,
        wall_s: finished.seconds,
        metrics: metrics::read(&sessions)?,
    })
}

async fn agent_then_verifier(
    env: &mut Env,
    setup: &AgentSetup<'_>,
    task: &Task,
    config: &Config,
    max_requests: usize,
    dir: &Path,
) -> Result<(Finished, Verdict)> {
    // Behind the relay the agent has no network but loopback.
    if matches!(setup.route, Route::Socket(_)) {
        env.set_network(false).await?;
    }
    let command = agent_command(setup, max_requests, &task.spec.prompt);
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let vars: Vec<String> = setup.api_key_env.map(str::to_owned).into_iter().collect();
    let timeout = Duration::from_secs_f64(config.agent_timeout_s);
    let finished = env
        .exec(
            &command,
            config.agent_user.as_deref(),
            &vars,
            timeout,
            &dir.join("events.jsonl"),
            Some(&dir.join("agent.log")),
        )
        .await?;
    env.resume().await?;
    fs::write(dir.join("agent.patch"), env.agent_patch().await?)?;
    let verdict = verify(env, &task.dir, config, &dir.join("checks")).await?;
    Ok((finished, verdict))
}

/// The task's images, made once per run.
async fn images_for(context: &RunContext, task: &Task, config: &Config) -> Result<Images> {
    let mut made = context.images.lock().await;
    if let Some(images) = made.get(task.id()) {
        return Ok(images.clone());
    }
    let dir = context.run_dir.join(task.id());
    fs::create_dir_all(&dir)?;
    let images = image(task.id(), &task.dir, config, &dir.join("image.log")).await?;
    made.insert(task.id().to_owned(), images.clone());
    Ok(images)
}

/// Validates a Harbor task: the verifier on the untouched environment, then on one where
/// solution/solve.sh has run. Returns both rewards.
pub async fn validate(
    task: &Task,
    config: &Config,
    dir: &Path,
) -> Result<(Option<f64>, Option<f64>)> {
    fs::create_dir_all(dir)?;
    let images = image(task.id(), &task.dir, config, &dir.join("image.log")).await?;

    let mut env = Env::start(&images.tag, config, None).await?;
    let start = verify(&mut env, &task.dir, config, &dir.join("start")).await;
    env.remove().await;
    let start = start?;

    let mut env = Env::start(&images.tag, config, None).await?;
    let solved = solve_and_verify(&mut env, task, config, dir).await;
    env.remove().await;
    Ok((start.reward, solved?.reward))
}

async fn solve_and_verify(
    env: &mut Env,
    task: &Task,
    config: &Config,
    dir: &Path,
) -> Result<Verdict> {
    env.copy_in(&task.dir.join("solution"), "/solution").await?;
    let vars: Vec<String> =
        config.solution_env.iter().map(|(k, v)| format!("{k}={}", expand(v))).collect();
    let timeout = Duration::from_secs_f64(config.agent_timeout_s);
    let log = dir.join("solve.log");
    env.exec(&["bash", "/solution/solve.sh"], None, &vars, timeout, &log, None).await?;
    env.resume().await?;
    verify(env, &task.dir, config, &dir.join("solved")).await
}

/// Runs `docker args`, logging both streams to `log`.
async fn docker(args: &[&str], timeout: Duration, log: &Path) -> Result<Finished> {
    run_logged(args, timeout, log, None).await
}

async fn run_logged(
    args: &[&str],
    timeout: Duration,
    stdout: &Path,
    stderr: Option<&Path>,
) -> Result<Finished> {
    let out = fs::File::create(stdout).with_context(|| format!("creating {}", stdout.display()))?;
    let err = match stderr {
        Some(path) => {
            fs::File::create(path).with_context(|| format!("creating {}", path.display()))?
        }
        None => out.try_clone()?,
    };
    let mut cmd = Command::new("docker");
    cmd.args(args).stdin(Stdio::null()).stdout(out).stderr(err).kill_on_drop(true);
    let started = Instant::now();
    let mut child = cmd.spawn().context("starting docker")?;
    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (Some(status?), false),
        Err(_) => {
            let _ = child.kill().await;
            (None, true)
        }
    };
    Ok(Finished {
        code: status.and_then(|s| s.code()),
        timed_out,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// Runs `docker args` to completion and returns its exit code and output.
async fn output(args: &[&str]) -> Result<(i32, String)> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("running docker")?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.code().unwrap_or(-1), text))
}

async fn quiet(args: &[&str]) -> Result<i32> {
    let status = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("running docker")?;
    Ok(status.code().unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_old_and_new_task_files() {
        let dir = std::env::temp_dir().join(format!("eidra-harbor-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("environment")).unwrap();
        fs::write(dir.join("environment/Dockerfile"), "FROM python:3.13-slim\n").unwrap();

        fs::write(
            dir.join("task.toml"),
            "version = \"1.0\"\n[metadata]\nsource = \"x::y\"\n[verifier]\ntimeout_sec = 3000\n\
             [agent]\ntimeout_sec = 3000\n[environment]\nbuild_timeout_sec = 1800.0\ncpus = 1\n\
             memory = '8G'\nstorage = '16G'\n",
        )
        .unwrap();
        let old = Config::load(&dir).unwrap();
        assert_eq!((old.agent_timeout_s, old.verifier_timeout_s), (3000.0, 3000.0));
        assert_eq!(old.memory_mb, Some(8192));
        assert!(old.verifier_network);
        assert_eq!(old.source.as_deref(), Some("x::y"));

        fs::write(
            dir.join("task.toml"),
            "schema_version = \"1.3\"\nartifacts = []\n[task]\nname = \"a/b\"\n[verifier]\n\
             timeout_sec = 60.0\ncollect = []\n[verifier.env]\nK = \"${EIDRA_NO_SUCH_VAR:-dflt}\"\n\
             [environment]\nnetwork_mode = \"no-network\"\nmemory_mb = 4096\nworkdir = \"/app\"\n\
             [environment.env]\n[solution.env]\n",
        )
        .unwrap();
        let new = Config::load(&dir).unwrap();
        assert_eq!(new.agent_timeout_s, AGENT_TIMEOUT_S);
        assert!(!new.verifier_network);
        assert_eq!(new.workdir.as_deref(), Some("/app"));
        assert_eq!(expand(&new.verifier_env["K"]), "dflt");

        fs::write(dir.join("task.toml"), "[verifier]\nenvironment_mode = \"separate\"\n").unwrap();
        assert!(Config::load(&dir).unwrap_err().to_string().contains("separate"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sizes_names_and_rewards() {
        assert_eq!(megabytes("4G"), Some(4096));
        assert_eq!(megabytes("512M"), Some(512));
        assert_eq!(megabytes("2Gi"), None);
        assert_eq!(image_name("HKUDS__nanobot-4048"), "hkuds__nanobot-4048");

        let dir = std::env::temp_dir().join(format!("eidra-harbor-reward-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Dockerfile"), "# c\nFROM --platform=linux/amd64 a/b:1 AS x\n").unwrap();
        assert_eq!(base_image(&dir.join("Dockerfile")).as_deref(), Some("a/b:1"));
        fs::write(dir.join("reward.txt"), "1\n").unwrap();
        assert_eq!(read_reward(&dir), Some(1.0));
        fs::write(dir.join("reward.json"), r#"{"tests": 1.0, "style": 0.5}"#).unwrap();
        assert_eq!(read_reward(&dir), Some(0.5));
        fs::remove_dir_all(dir).unwrap();
    }
}
