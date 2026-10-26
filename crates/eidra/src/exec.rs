//! `eidra exec`: one task, unattended (docs/DESIGN.md §15). Progress goes to stderr, or as JSON
//! lines to stdout with --json; the model's final summary goes to stdout.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use eidra_core::{
    Agent, AgentOptions, Approval, Decision, Event, Frontend, Outcome, Profile, RequestStats,
};
use serde_json::{Value, json};

use crate::format::thousands;

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// The task. Without it, the task is read from --task-file or stdin
    task: Option<String>,

    /// Read the task from this file
    #[arg(long, value_name = "FILE")]
    task_file: Option<PathBuf>,

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

    /// Reach the model server through this Unix socket instead of the network, for an agent
    /// with no network of its own (see `eidra relay`)
    #[arg(long, env = "EIDRA_MODEL_SOCKET", value_name = "PATH")]
    model_socket: Option<PathBuf>,

    /// Stop after this many requests to the model
    #[arg(long, default_value_t = 150)]
    max_requests: usize,

    /// A command that needs approval, on top of the profile's; repeatable. An unattended run
    /// declines it
    #[arg(long = "confirm", value_name = "PATTERN")]
    confirm: Vec<String>,

    /// Where session logs go [default: $XDG_DATA_HOME/eidra/sessions]
    #[arg(long, env = "EIDRA_SESSION_DIR", value_name = "DIR")]
    session_dir: Option<PathBuf>,

    /// Print events as JSON lines on stdout
    #[arg(long)]
    json: bool,
}

/// Exit codes besides 0 (finished) and 1 (error).
const REQUEST_LIMIT: i32 = 2;
const CONTEXT_LIMIT: i32 = 3;
const REPLIED: i32 = 4;

pub async fn run(args: ExecArgs) -> Result<i32> {
    let task = read_task(&args)?;
    let mut profile = match &args.profile {
        Some(dir) => Profile::load(dir)?,
        None => Profile::builtin()?,
    };
    if let Some(url) = &args.base_url {
        profile.base_url.clone_from(url);
    }
    if let Some(model) = &args.model {
        profile.model.clone_from(model);
    }
    let workspace = match &args.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("the current directory")?,
    };
    let session_root = args.session_dir.clone().unwrap_or_else(|| default_session_dir(&workspace));
    let options = AgentOptions {
        workspace,
        session_root,
        headless: true,
        confirm: args.confirm.clone(),
        model_socket: args.model_socket.clone(),
    };
    let mut agent = Agent::start(profile, options)?;

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

/// $XDG_DATA_HOME/eidra/sessions, else ~/.local/share/eidra/sessions, else .eidra/sessions in the
/// workspace (a container user without a home).
fn default_session_dir(workspace: &Path) -> PathBuf {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(data).join("eidra/sessions");
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() && home != "/" => {
            PathBuf::from(home).join(".local/share/eidra/sessions")
        }
        _ => workspace.join(".eidra/sessions"),
    }
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

pub(crate) fn describe_stats(stats: &RequestStats) -> String {
    let mut parts = Vec::new();
    if let Some(prompt) = stats.prompt_tokens {
        let cached = stats.cached_tokens.unwrap_or(0);
        let share = (100 * cached).checked_div(prompt).unwrap_or(0);
        parts.push(format!("{} prompt ({share}% cached)", thousands(prompt)));
    }
    if let Some(out) = stats.completion_tokens {
        match stats.reasoning_tokens {
            Some(thinking) if thinking > 0 => {
                parts.push(format!("{} out ({} thinking)", thousands(out), thousands(thinking)))
            }
            _ => parts.push(format!("{} out", thousands(out))),
        }
    }
    if let Some(ttft) = stats.ttft_ms {
        parts.push(format!("{:.1} s to first token", ttft / 1000.0));
    }
    if let Some(rate) = stats.decode_per_second {
        parts.push(format!("{rate:.0} tok/s"));
    }
    if let Some(reason) =
        stats.finish_reason.as_deref().filter(|r| *r != "tool_calls" && *r != "stop")
    {
        parts.push(format!("stopped: {reason}"));
    }
    parts.join(" · ")
}

/// A one-line description of a tool call.
pub(crate) fn describe_call(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let number = |key: &str| args.get(key).map(|v| v.to_string().trim_matches('"').to_owned());
    match name {
        "bash" => {
            let command = text("command");
            let mut line = clip(command.lines().next().unwrap_or_default(), 160);
            if command.lines().count() > 1 {
                line.push_str(" …");
            }
            let background = args.get("background").is_some_and(|b| b == true || b == "true");
            format!("$ {line}{}", if background { "  (background)" } else { "" })
        }
        "read" => {
            let range = match (number("offset"), number("limit")) {
                (None, None) => String::new(),
                (offset, limit) => format!(
                    " (from line {}{})",
                    offset.unwrap_or_else(|| "1".into()),
                    limit.map(|l| format!(", {l} lines")).unwrap_or_default()
                ),
            };
            format!("read {}{range}", text("path"))
        }
        "edit" => format!("edit {}", text("path")),
        "write" => format!("write {} ({} lines)", text("path"), text("content").lines().count()),
        "glob" => format!("glob {}", text("pattern")),
        "grep" => format!("grep {} {}", text("pattern"), text("path")).trim_end().to_owned(),
        "bash_output" | "bash_kill" => format!("{name} {}", number("id").unwrap_or_default()),
        "task" => format!("task: {}", text("description")),
        "todo" | "finish" => name.to_owned(),
        _ => format!("{name} {}", clip(arguments, 160)),
    }
}

/// The first line of a tool's output, and how many more there are.
fn summarise(output: &str) -> String {
    let lines = output.lines().count();
    let first = clip(output.lines().next().unwrap_or("(no output)"), 140);
    if lines > 1 { format!("{first}  (+{} lines)", lines - 1) } else { first }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_calls_in_one_line() {
        assert_eq!(
            describe_call("bash", r#"{"command": "cd x && make\nmake test"}"#),
            "$ cd x && make …"
        );
        assert_eq!(
            describe_call("bash", r#"{"command": "serve", "background": true}"#),
            "$ serve  (background)"
        );
        assert_eq!(
            describe_call("read", r#"{"path": "a.py", "offset": 40}"#),
            "read a.py (from line 40)"
        );
        assert_eq!(
            describe_call("write", r#"{"path": "a.py", "content": "x\ny\n"}"#),
            "write a.py (2 lines)"
        );
        assert_eq!(describe_call("grep", r#"{"pattern": "def main"}"#), "grep def main");
        assert_eq!(
            describe_call("task", r#"{"description": "find the parser", "prompt": "…"}"#),
            "task: find the parser"
        );
    }

    #[test]
    fn summarises_output_and_stats() {
        assert_eq!(summarise("one\ntwo\nthree"), "one  (+2 lines)");
        let stats = RequestStats {
            prompt_tokens: Some(12_000),
            cached_tokens: Some(9_000),
            completion_tokens: Some(300),
            reasoning_tokens: Some(120),
            ttft_ms: Some(1_250.0),
            decode_per_second: Some(118.4),
            finish_reason: Some("tool_calls".into()),
            ..Default::default()
        };
        assert_eq!(
            describe_stats(&stats),
            "12,000 prompt (75% cached) · 300 out (120 thinking) · 1.2 s to first token · 118 tok/s"
        );
    }
}
