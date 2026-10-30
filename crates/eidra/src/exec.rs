//! `eidra exec`: one task, unattended (docs/DESIGN.md §15). Progress goes to stderr, or as JSON
//! lines to stdout with --json; the model's final summary goes to stdout.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use eidra_core::display::{describe_call, describe_stats, summarise};
use eidra_core::{
    Agent, AgentOptions, Approval, Decision, Event, Frontend, Outcome, Profile, eidra_home,
    list_sessions,
};
use serde_json::{Value, json};

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// The task. Without it, the task is read from --task-file or stdin
    task: Option<String>,

    /// Read the task from this file
    #[arg(long, value_name = "FILE")]
    task_file: Option<PathBuf>,

    #[command(flatten)]
    agent: AgentArgs,

    /// Reach the model server through this Unix socket instead of the network, for an agent
    /// with no network of its own (see `eidra relay`)
    #[arg(long, env = "EIDRA_MODEL_SOCKET", value_name = "PATH")]
    model_socket: Option<PathBuf>,

    /// Stop after this many requests to the model
    #[arg(long, default_value_t = 150)]
    max_requests: usize,

    /// Seconds the run has, as whoever runs it allows: near the end the model is told to finish.
    /// eidra does not stop at it
    #[arg(long, value_name = "SECONDS")]
    time_limit: Option<u64>,

    /// Print events as JSON lines on stdout
    #[arg(long)]
    json: bool,
}

/// What an agent's session starts from, for `eidra exec` and the interactive `eidra`.
#[derive(Debug, Args)]
pub struct AgentArgs {
    /// The directory to work in [default: the current directory]
    #[arg(short = 'C', long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// A profile directory, with profile.toml and system.md [default: the built-in qwen3.8-radiance]
    #[arg(long, env = "EIDRA_PROFILE", value_name = "DIR")]
    profile: Option<PathBuf>,

    /// The OpenAI-compatible API root, instead of the profile's
    #[arg(long, env = "EIDRA_BASE_URL")]
    base_url: Option<String>,

    /// The model, instead of the profile's
    #[arg(long, env = "EIDRA_MODEL")]
    model: Option<String>,

    /// A command that needs approval, on top of the profile's; repeatable. An unattended run
    /// declines it
    #[arg(long = "confirm", value_name = "PATTERN")]
    pub confirm: Vec<String>,

    /// Where session logs go [default: ~/.eidra/sessions]
    #[arg(long, env = "EIDRA_SESSION_DIR", value_name = "DIR")]
    session_dir: Option<PathBuf>,

    /// Go on with the latest session in this directory
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    continue_session: bool,

    /// Go on with this session: its id, as `eidra sessions` lists it, or the start of one
    #[arg(short = 'r', long, value_name = "ID")]
    resume: Option<String>,
}

impl AgentArgs {
    /// The profile, with the server and model the command line gives.
    pub fn profile(&self) -> Result<Profile> {
        let mut profile = match &self.profile {
            Some(dir) => Profile::load(dir)?,
            None => Profile::builtin()?,
        };
        if let Some(url) = &self.base_url {
            profile.base_url.clone_from(url);
        }
        if let Some(model) = &self.model {
            profile.model.clone_from(model);
        }
        Ok(profile)
    }

    pub fn workspace(&self) -> Result<PathBuf> {
        match &self.cwd {
            Some(dir) => Ok(dir.clone()),
            None => std::env::current_dir().context("the current directory"),
        }
    }

    pub fn session_root(&self, workspace: &Path) -> PathBuf {
        self.session_dir.clone().unwrap_or_else(|| default_session_dir(workspace))
    }

    /// The log of the session to go on with, if --continue or --resume asks for one: the latest
    /// of this directory's sessions of the same kind, or the one with that id.
    pub fn session_to_resume(&self, workspace: &Path, headless: bool) -> Result<Option<PathBuf>> {
        let root = self.session_root(workspace);
        if let Some(id) = &self.resume {
            let exact = root.join(format!("{id}.jsonl"));
            if exact.is_file() {
                return Ok(Some(exact));
            }
            let sessions = list_sessions(&root);
            let matching: Vec<_> =
                sessions.iter().filter(|s| s.header.id.starts_with(id)).collect();
            return match matching.as_slice() {
                [one] => Ok(Some(one.path.clone())),
                [] => bail!("no session {id} in {}; `eidra sessions` lists them", root.display()),
                _ => bail!("{} sessions start with {id}; give more of the id", matching.len()),
            };
        }
        if !self.continue_session {
            return Ok(None);
        }
        let here = workspace.canonicalize().context("the current directory")?;
        let latest = list_sessions(&root)
            .into_iter()
            .find(|s| s.header.headless == headless && Path::new(&s.header.workspace) == here);
        match latest {
            Some(session) => Ok(Some(session.path)),
            None => {
                let kind = if headless { "unattended" } else { "interactive" };
                bail!("no {kind} session in {} to go on with", here.display())
            }
        }
    }
}

/// Exit codes besides 0 (finished) and 1 (error).
const REQUEST_LIMIT: i32 = 2;
const CONTEXT_LIMIT: i32 = 3;
const REPLIED: i32 = 4;
/// As a shell reports a command stopped by Ctrl-C.
const INTERRUPTED: i32 = 130;

pub async fn run(args: ExecArgs) -> Result<i32> {
    let task = read_task(&args)?;
    let profile = args.agent.profile()?;
    let workspace = args.agent.workspace()?;
    let options = AgentOptions {
        session_root: args.agent.session_root(&workspace),
        workspace,
        headless: true,
        confirm: args.agent.confirm.clone(),
        model_socket: args.model_socket.clone(),
        // Used only by a profile that gives unattended runs the skill tool, which the built-in
        // one does not.
        skills: eidra_core::skills::dir(),
    };
    let mut agent = match args.agent.session_to_resume(&options.workspace, true)? {
        Some(log) => Agent::resume(profile, options, &log)?,
        None => Agent::start(profile, options)?,
    };

    if let Some(seconds) = args.time_limit {
        agent.set_time_limit(std::time::Duration::from_secs(seconds));
    }
    let mut output = Output::new(args.json);
    output.start(&agent);
    agent.push_user(&task)?;
    let outcome = agent.run(&mut output, args.max_requests).await?;
    output.end(&outcome, agent.session_log());
    Ok(match outcome {
        Outcome::Finished(_) => 0,
        Outcome::RequestLimit => REQUEST_LIMIT,
        Outcome::ContextLimit(_) => CONTEXT_LIMIT,
        Outcome::Replied => REPLIED,
        Outcome::Interrupted => INTERRUPTED,
    })
}

fn read_task(args: &ExecArgs) -> Result<String> {
    let task = match (&args.task, &args.task_file) {
        (Some(task), None) => task.clone(),
        (None, Some(file)) => {
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?
        }
        (Some(_), Some(_)) => bail!("give the task as an argument or with --task-file, not both"),
        (None, None) if !std::io::stdin().is_terminal() => {
            let mut task = String::new();
            std::io::stdin().read_to_string(&mut task).context("reading the task from stdin")?;
            task
        }
        (None, None) => bail!("give the task as an argument, with --task-file, or on stdin"),
    };
    if task.trim().is_empty() {
        bail!("the task is empty");
    }
    Ok(task.trim().to_owned())
}

/// `sessions` in eidra's own directory: ~/.eidra/sessions, unless $EIDRA_HOME moves it.
pub fn default_session_dir(workspace: &Path) -> PathBuf {
    eidra_home(workspace).join("sessions")
}

/// Headless output. Nobody can approve a command, so those are declined.
struct Output {
    json: bool,
    styled: bool,
    /// What is being streamed: reasoning or reply text, so the next thing starts on a new line.
    streaming: Option<Stream>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Reasoning,
    Content,
}

impl Output {
    fn new(json: bool) -> Self {
        Output { json, styled: std::io::stderr().is_terminal(), streaming: None }
    }

    fn start(&mut self, agent: &Agent) {
        let profile = agent.profile();
        if self.json {
            self.print_json(&json!({
                "type": "start",
                "session": agent.session_id(),
                "log": agent.session_log(),
                "workspace": agent.workspace(),
                "model": profile.model,
                "base_url": profile.base_url,
                "profile": profile.name,
                "profile_hash": profile.hash,
            }));
        } else {
            eprintln!(
                "{}",
                self.dim(&format!(
                    "eidra exec · {} at {} · {} · log {}",
                    profile.model,
                    profile.base_url,
                    agent.workspace().display(),
                    agent.session_log().display()
                ))
            );
        }
    }

    fn end(&mut self, outcome: &Outcome, log: &Path) {
        self.stop_streaming();
        if self.json {
            let (kind, detail) = match outcome {
                Outcome::Finished(summary) => ("finished", json!(summary)),
                Outcome::Replied => ("replied", Value::Null),
                Outcome::RequestLimit => ("request_limit", Value::Null),
                Outcome::ContextLimit(tokens) => ("context_limit", json!(tokens)),
                Outcome::Interrupted => ("interrupted", Value::Null),
            };
            self.print_json(&json!({"type": "end", "outcome": kind, "detail": detail, "log": log}));
            return;
        }
        match outcome {
            Outcome::Finished(summary) => println!("{summary}"),
            Outcome::Replied => eprintln!("The model replied without finishing."),
            Outcome::RequestLimit => eprintln!("Stopped: the request limit was reached."),
            Outcome::ContextLimit(tokens) => {
                eprintln!("Stopped: the conversation reached {tokens} tokens.")
            }
            Outcome::Interrupted => eprintln!("Interrupted."),
        }
    }

    fn print_json(&self, value: &Value) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{value}");
        let _ = stdout.flush();
    }

    fn stream(&mut self, kind: Stream, text: &str) {
        if self.streaming != Some(kind) {
            self.stop_streaming();
            if kind == Stream::Reasoning && self.styled {
                eprint!("\x1b[2m");
            }
            self.streaming = Some(kind);
        }
        eprint!("{text}");
        let _ = std::io::stderr().flush();
    }

    fn stop_streaming(&mut self) {
        if let Some(kind) = self.streaming.take() {
            if kind == Stream::Reasoning && self.styled {
                eprint!("\x1b[0m");
            }
            eprintln!();
        }
    }

    fn dim(&self, text: &str) -> String {
        if self.styled { format!("\x1b[2m{text}\x1b[0m") } else { text.to_owned() }
    }
}

impl Frontend for Output {
    fn event(&mut self, event: Event) {
        if self.json {
            if let Ok(value) = serde_json::to_value(&event) {
                self.print_json(&value);
            }
            return;
        }
        match event {
            Event::Request { .. } => {}
            Event::Reasoning { text } => self.stream(Stream::Reasoning, &text),
            Event::Content { text } => self.stream(Stream::Content, &text),
            Event::Reply { stats } => {
                self.stop_streaming();
                eprintln!("{}", self.dim(&format!("  · {}", describe_stats(&stats))));
            }
            Event::ToolCall { name, arguments, .. } => {
                eprintln!("→ {}", describe_call(&name, &arguments))
            }
            Event::ToolResult { output, is_error, .. } => {
                let mark = if is_error { "✗" } else { "←" };
                eprintln!("{}", self.dim(&format!("  {mark} {}", summarise(&output))));
            }
            Event::Todo { list } => {
                for line in list.lines() {
                    eprintln!("  {line}");
                }
            }
            Event::Notice { text } => {
                self.stop_streaming();
                eprintln!("! {text}");
            }
            // Subagents run side by side, so only their steps are shown, each under its task.
            Event::Subagent { label, event, .. } => match *event {
                Event::ToolCall { name, arguments, .. } => {
                    self.stop_streaming();
                    eprintln!("  [{label}] → {}", describe_call(&name, &arguments));
                }
                Event::ToolResult { output, is_error, .. } => {
                    let mark = if is_error { "✗" } else { "←" };
                    eprintln!(
                        "{}",
                        self.dim(&format!("  [{label}]   {mark} {}", summarise(&output)))
                    );
                }
                Event::Notice { text } => {
                    self.stop_streaming();
                    eprintln!("  [{label}] ! {text}");
                }
                _ => {}
            },
        }
    }

    async fn approve(&mut self, _approval: &Approval) -> Decision {
        Decision::Deny {
            note: Some(
                "Nobody can approve commands in this unattended session: find another way, or leave \
                 the step out and say so in your summary."
                    .into(),
            ),
        }
    }
}
