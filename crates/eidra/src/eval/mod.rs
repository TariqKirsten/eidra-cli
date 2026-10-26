//! `eidra eval`: runs a task suite against the agent and reports how it did (docs/DESIGN.md §15,
//! evals/README.md).
//!
//! Each attempt gets:
//! - a fresh copy of the task's workspace
//! - its own container, with the static eidra binary mounted in
//!
//! The checks then run in another container, with no network, on a copy of what the agent left,
//! with the task's hidden check/ directory mounted.

mod docker;
mod metrics;
mod report;
mod task;

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use eidra_core::{Profile, fnv1a, utc_datetime};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use docker::{Container, copy_dir};
use metrics::Metrics;
use task::{Split, Task, load_suite};

#[derive(Debug, Args)]
pub struct EvalArgs {
    #[command(subcommand)]
    command: EvalCommand,
}

#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Run the suite's tasks against the agent and record the results
    Run(RunArgs),
    /// Print the report of a recorded run
    Report {
        /// The run's directory
        run: PathBuf,
    },
    /// Compare two recorded runs, task by task
    Compare {
        /// The run to compare against
        before: PathBuf,
        /// The run being judged
        after: PathBuf,
    },
    /// Check the tasks themselves, without the model: their checks fail on the starting workspace
    /// and pass on the solution
    Validate(Selection),
    /// Run the suite's current checks again on what each attempt of a recorded run left, after a
    /// check has been fixed. The results as they were are kept in results.before-regrade.jsonl
    Regrade {
        /// The run's directory
        run: PathBuf,
        /// The suite the checks come from
        #[arg(long, default_value = "evals/tasks", value_name = "DIR")]
        suite: PathBuf,
    },
}

#[derive(Debug, Args)]
struct Selection {
    /// The suite: a directory of task directories
    #[arg(long, default_value = "evals/tasks", value_name = "DIR")]
    suite: PathBuf,

    /// Only this task; repeatable
    #[arg(long = "task", value_name = "ID")]
    tasks: Vec<String>,

    /// Only the tasks in this split
    #[arg(long, value_enum)]
    split: Option<Split>,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    selection: Selection,

    /// Attempts per task
    #[arg(long, default_value_t = 3)]
    repeats: u32,

    /// Attempts at a time. Radiance serves four requests at once; running more than one attempt
    /// makes the timings noisier
    #[arg(long, default_value_t = 1)]
    parallel: usize,

    /// A profile directory [default: the built-in profile]
    #[arg(long, value_name = "DIR")]
    profile: Option<PathBuf>,

    /// A name for the run [default: the profile's name]
    #[arg(long)]
    label: Option<String>,

    /// Where runs are recorded
    #[arg(long, default_value = "evals/runs", value_name = "DIR")]
    out: PathBuf,
}

/// What a run was, written when it starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub label: String,
    pub started: String,
    pub eidra: String,
    /// A hash of the eidra binary the attempts ran, which tells builds of one version apart.
    #[serde(default)]
    pub binary_hash: String,
    pub profile: String,
    pub profile_hash: String,
    pub model: String,
    pub base_url: String,
    pub suite: String,
    pub tasks: Vec<String>,
    pub repeats: u32,
    pub parallel: usize,
    /// When the checks were last run again on the attempts (`eidra eval regrade`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regraded: Option<String>,
}

/// One attempt at one task: a line of results.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub task: String,
    pub split: Split,
    pub repeat: u32,
    pub passed: bool,
    pub checks: Vec<CheckResult>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub wall_s: f64,
    #[serde(flatten)]
    pub metrics: Metrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub timed_out: bool,
    pub seconds: f64,
}

/// What every attempt of a run shares.
struct RunContext {
    binary: PathBuf,
    profile_dir: Option<PathBuf>,
    api_key_env: Option<String>,
    run_dir: PathBuf,
}

pub async fn run(args: EvalArgs) -> Result<i32> {
    match args.command {
        EvalCommand::Run(args) => run_suite(args).await,
        EvalCommand::Report { run } => {
            let (manifest, attempts) = load_run(&run)?;
            report::print(&manifest, &attempts);
            Ok(0)
        }
        EvalCommand::Compare { before, after } => {
            let before = load_run(&before)?;
            let after = load_run(&after)?;
            report::compare(&before, &after);
            Ok(0)
        }
        EvalCommand::Validate(selection) => validate(&selection).await,
        EvalCommand::Regrade { run, suite } => regrade(&run, &suite).await,
    }
}

fn select(selection: &Selection) -> Result<Vec<Task>> {
    let suite = fs::canonicalize(&selection.suite)
        .with_context(|| format!("the suite {}", selection.suite.display()))?;
    let mut tasks = load_suite(&suite)?;
    for id in &selection.tasks {
        if !tasks.iter().any(|t| t.id() == id) {
            bail!("there is no task {id} in {}", suite.display());
        }
    }
    if !selection.tasks.is_empty() {
        tasks.retain(|t| selection.tasks.iter().any(|id| id == t.id()));
    }
    if let Some(split) = selection.split {
        tasks.retain(|t| t.spec.split == split);
    }
    if tasks.is_empty() {
        bail!("no task matches");
    }
    Ok(tasks)
}

async fn run_suite(args: RunArgs) -> Result<i32> {
    let tasks = select(&args.selection)?;
    let (profile, profile_dir) = match &args.profile {
        Some(dir) => (Profile::load(dir)?, Some(fs::canonicalize(dir)?)),
        None => (Profile::builtin()?, None),
    };
    if let Some(var) = &profile.api_key_env
        && std::env::var_os(var).is_none()
    {
        eprintln!("note: ${var} is not set, so the agent sends no API key");
    }
    let label = args.label.clone().unwrap_or_else(|| profile.name.clone());
    let (compact_stamp, iso_stamp) = now();
    let run_dir = args.out.join(format!("{compact_stamp}-{}", sanitize(&label)));
    fs::create_dir_all(&run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
    let run_dir = fs::canonicalize(&run_dir)?;

    let binary = std::env::current_exe().context("finding the eidra binary")?;
    let binary_bytes =
        fs::read(&binary).with_context(|| format!("reading {}", binary.display()))?;
    let manifest = Manifest {
        label,
        started: iso_stamp,
        eidra: env!("CARGO_PKG_VERSION").into(),
        binary_hash: format!("{:016x}", fnv1a(&binary_bytes)),
        profile: profile.name.clone(),
        profile_hash: profile.hash.clone(),
        model: profile.model.clone(),
        base_url: profile.base_url.clone(),
        suite: args.selection.suite.display().to_string(),
        tasks: tasks.iter().map(|t| t.id().to_owned()).collect(),
        repeats: args.repeats,
        parallel: args.parallel.max(1),
        regraded: None,
    };
    fs::write(run_dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    eprintln!(
        "eval run {} · {} tasks × {} attempts · profile {} ({}) · binary {}",
        run_dir.display(),
        tasks.len(),
        args.repeats,
        profile.name,
        &profile.hash[..8],
        &manifest.binary_hash[..8]
    );

    let context = Arc::new(RunContext {
        binary,
        profile_dir,
        api_key_env: profile.api_key_env.clone(),
        run_dir: run_dir.clone(),
    });
    let tasks = Arc::new(tasks);
    // Every task once, then every task again: a run stopped early still covers the whole suite.
    let jobs: Vec<(usize, u32)> = (1..=args.repeats)
        .flat_map(|repeat| (0..tasks.len()).map(move |task| (task, repeat)))
        .collect();
    let mut results =
        OpenOptions::new().create(true).append(true).open(run_dir.join("results.jsonl"))?;
    let mut running = JoinSet::new();
    let mut next = 0;
    let mut attempts = Vec::new();
    while next < jobs.len() || !running.is_empty() {
        while next < jobs.len() && running.len() < manifest.parallel {
            let (task, repeat) = jobs[next];
            next += 1;
            let (tasks, context) = (Arc::clone(&tasks), Arc::clone(&context));
            running.spawn(async move { run_attempt(&context, &tasks[task], repeat).await });
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let attempt = joined.context("an attempt panicked")??;
        writeln!(results, "{}", serde_json::to_string(&attempt)?)?;
        report::progress(attempts.len() + 1, jobs.len(), &attempt);
        attempts.push(attempt);
    }
    eprintln!();
    report::print(&manifest, &attempts);
    Ok(0)
}

async fn run_attempt(context: &RunContext, task: &Task, repeat: u32) -> Result<Attempt> {
    let dir = context.run_dir.join(task.id()).join(repeat.to_string());
    let work = dir.join("work");
    let sessions = dir.join("sessions");
    copy_dir(&task.workspace(), &work)?;
    fs::create_dir_all(&sessions)?;

    let max_requests = task.spec.max_requests.to_string();
    let mut command = vec![
        "eidra",
        "exec",
        "--json",
        "--session-dir",
        "/sessions",
        "--max-requests",
        &max_requests,
    ];
    let mut agent = Container::new(&task.spec.image, "host")
        .mount(&context.binary, "/usr/local/bin/eidra", true)
        .mount(&work, "/work", false)
        .mount(&sessions, "/sessions", false);
    if let Some(profile) = &context.profile_dir {
        agent = agent.mount(profile, "/profile", true);
        command.extend(["--profile", "/profile"]);
    }
    if let Some(var) = &context.api_key_env {
        agent = agent.pass_env(var);
    }
    command.push(&task.spec.prompt);
    let timeout = Duration::from_secs(task.spec.timeout_s);
    let finished = agent
        .run(&command, timeout, &dir.join("events.jsonl"), Some(&dir.join("agent.log")))
        .await?;

    let metrics = metrics::read(&sessions)?;
    let checks = run_checks(task, &work, &dir).await?;
    Ok(Attempt {
        task: task.id().to_owned(),
        split: task.spec.split,
        repeat,
        passed: checks.iter().all(|c| c.passed),
        checks,
        exit_code: finished.code,
        timed_out: finished.timed_out,
        wall_s: finished.seconds,
        metrics,
    })
}

/// Runs a task's checks on a copy of `workspace`, logging each to `dir/checks/<name>.log`.
async fn run_checks(task: &Task, workspace: &Path, dir: &Path) -> Result<Vec<CheckResult>> {
    let scratch = dir.join("check-work");
    let _ = fs::remove_dir_all(&scratch);
    copy_dir(workspace, &scratch)?;
    let logs = dir.join("checks");
    fs::create_dir_all(&logs)?;
    let mut container = Container::new(&task.spec.image, "none").mount(&scratch, "/work", false);
    if let Some(check_dir) = task.check_dir() {
        container = container.mount(&check_dir, "/check", true);
    }
    let mut results = Vec::new();
    for check in &task.spec.checks {
        let log = logs.join(format!("{}.log", check.name));
        let timeout = Duration::from_secs(check.timeout_s);
        let finished = container.run(&["sh", "-c", &check.run], timeout, &log, None).await?;
        results.push(CheckResult {
            name: check.name.clone(),
            passed: finished.code == Some(0) && !finished.timed_out,
            timed_out: finished.timed_out,
            seconds: finished.seconds,
        });
    }
    fs::remove_dir_all(&scratch)?;
    Ok(results)
}

/// Every task's checks must fail on its starting workspace, or the task tests nothing, and pass on
/// its solution, or the task cannot be done.
async fn validate(selection: &Selection) -> Result<i32> {
    let tasks = select(selection)?;
    let root = std::env::temp_dir().join(format!("eidra-validate-{}", std::process::id()));
    let mut problems = 0;
    for task in &tasks {
        let dir = root.join(task.id());
        let start = dir.join("start");
        copy_dir(&task.workspace(), &start)?;
        let before = run_checks(task, &start, &dir.join("start-checks")).await?;
        let solved = dir.join("solved");
        copy_dir(&task.workspace(), &solved)?;
        if task.solution().is_dir() {
            copy_dir(&task.solution(), &solved)?;
        }
        let after = run_checks(task, &solved, &dir.join("solved-checks")).await?;

        let failing = |results: &[CheckResult]| -> Vec<String> {
            results.iter().filter(|c| !c.passed).map(|c| c.name.clone()).collect()
        };
        let (start_fails, solution_fails) = (failing(&before), failing(&after));
        let ok = !start_fails.is_empty() && solution_fails.is_empty();
        println!(
            "{:<18} {:<5}  start: {:<34} solution: {:<28} {}",
            task.id(),
            task.spec.split.name(),
            if start_fails.is_empty() {
                "PASSES EVERY CHECK".into()
            } else {
                format!("fails {}", start_fails.join(", "))
            },
            if solution_fails.is_empty() {
                "passes".into()
            } else {
                format!("FAILS {}", solution_fails.join(", "))
            },
            if ok { "ok" } else { "PROBLEM" }
        );
        if !ok {
            problems += 1;
            println!("  check logs: {}", dir.display());
        }
    }
    if problems == 0 {
        let _ = fs::remove_dir_all(&root);
    }
    Ok(i32::from(problems > 0))
}

/// Runs the current checks again on what each attempt left (its work/ directory, which the checks
/// never change) and rewrites the run's results.
async fn regrade(run: &Path, suite: &Path) -> Result<i32> {
    let (mut manifest, attempts) = load_run(run)?;
    let suite =
        fs::canonicalize(suite).with_context(|| format!("the suite {}", suite.display()))?;
    let tasks = load_suite(&suite)?;
    let mut regraded = Vec::new();
    for mut attempt in attempts {
        let name = format!("{} #{}", attempt.task, attempt.repeat);
        let Some(task) = tasks.iter().find(|t| t.id() == attempt.task) else {
            println!("{name:<20} no longer in the suite; left as it was");
            regraded.push(attempt);
            continue;
        };
        let dir = run.join(&attempt.task).join(attempt.repeat.to_string());
        let checks = run_checks(task, &dir.join("work"), &dir).await?;
        let passed = checks.iter().all(|c| c.passed);
        let verdict = |passed: bool, checks: &[CheckResult]| {
            let failed: Vec<&str> =
                checks.iter().filter(|c| !c.passed).map(|c| c.name.as_str()).collect();
            if passed { "pass".to_owned() } else { format!("FAIL ({})", failed.join(", ")) }
        };
        let (before, after) = (verdict(attempt.passed, &attempt.checks), verdict(passed, &checks));
        if before == after {
            println!("{name:<20} {after}");
        } else {
            println!("{name:<20} {before} → {after}");
        }
        attempt.passed = passed;
        attempt.checks = checks;
        regraded.push(attempt);
    }

    let results = run.join("results.jsonl");
    let original = run.join("results.before-regrade.jsonl");
    if !original.exists() {
        fs::copy(&results, &original)?;
    }
    let mut lines = String::new();
    for attempt in &regraded {
        lines += &(serde_json::to_string(attempt)? + "\n");
    }
    fs::write(&results, lines)?;
    manifest.regraded = Some(now().1);
    fs::write(run.join("manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    println!();
    report::print(&manifest, &regraded);
    Ok(0)
}

fn load_run(dir: &Path) -> Result<(Manifest, Vec<Attempt>)> {
    let manifest = fs::read_to_string(dir.join("manifest.json"))
        .with_context(|| format!("reading {}", dir.display()))?;
    let manifest: Manifest = serde_json::from_str(&manifest).context("manifest.json")?;
    let results = fs::read_to_string(dir.join("results.jsonl")).unwrap_or_default();
    let attempts = results
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("results.jsonl"))
        .collect::<Result<Vec<Attempt>>>()?;
    Ok((manifest, attempts))
}

/// `20261026-181500` and `2026-10-26T18:15:00Z`.
fn now() -> (String, String) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (y, mo, d, h, mi, s) = utc_datetime(secs);
    (
        format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}"),
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"),
    )
}

fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
        .collect()
}
