//! The interactive session: the loop that reads the keyboard, runs the agent and draws.
//!
//! The agent runs in a future that owns it and hands it back when the turn ends. Meanwhile it
//! talks to the loop through a channel: events to show, approval requests carrying a oneshot for
//! the answer, and subagents to run in the background. Those run as futures of their own, through
//! the agent's turns and between them, and each ends with a report for the agent. The loop selects
//! over the terminal's events, that channel, the turn, the background subagents and a spinner
//! tick, and draws at most once a frame.

use std::future::Future;
use std::io::{self, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TerminalEvent, EventStream, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode};
use eidra_core::checkpoint::{Checkpoints, Undone};
use eidra_core::display::{clip, compact, describe_call, duration, summarise, thousands};
use eidra_core::{
    Agent, AgentOptions, Approval, Background, Decision, Event, Frontend, Interrupter, Log,
    Outcome, Profile, Report, RequestStats, text_of,
};
use eidra_llm::Message as Logged;
use futures_util::StreamExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};

use crate::attach;
use crate::editor::Editor;
use crate::markdown;
use crate::screen::Screen;
use crate::style::{self, Line, Style, theme};
use crate::wrap::{wrap, wrap_all};

/// What sessions start from: the first one, and each `/new` after it.
#[derive(Clone)]
pub struct Options {
    pub profile: Profile,
    pub workspace: PathBuf,
    pub session_root: PathBuf,
    /// Commands that need approval, on top of the profile's.
    pub confirm: Vec<String>,
    /// Where the checkpoint repositories go, one per workspace; None turns checkpoints off.
    pub checkpoints: Option<PathBuf>,
    /// The log of a session to go on with, instead of starting one.
    pub resume: Option<PathBuf>,
}

/// Requests one turn may make before the turn goes back to the user.
const MAX_REQUESTS: usize = 500;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_STEP: Duration = Duration::from_millis(80);
/// The least time between two draws while text streams in. Keys draw at once.
const FRAME: Duration = Duration::from_millis(16);
/// How long an approval ignores y, a and n after it appears, so that keys typed for the input box
/// do not answer it.
const APPROVAL_GRACE: Duration = Duration::from_millis(400);
/// Lines of a diff /diff shows; the rest are counted.
const DIFF_LINES: usize = 400;
/// Messages of a resumed conversation shown when it starts; earlier ones are counted.
const HISTORY: usize = 80;
/// How long a command typed after `!` may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// Lines of such a command's output the scrollback shows, from its start and its end.
const COMMAND_HEAD: usize = 10;
const COMMAND_TAIL: usize = 30;

const HELP: &[(&str, &str)] = &[
    ("enter", "send; while the agent works, queue it for when the turn ends"),
    ("ctrl+enter", "interrupt the agent and send at once, with anything queued"),
    ("shift+enter", "new line (also alt+enter and ctrl+j)"),
    ("esc", "interrupt the agent"),
    ("ctrl+c", "interrupt; or clear the input; or quit when it is empty"),
    ("ctrl+d", "quit, when the input is empty"),
    (
        "up, down",
        "move between rows; from the first or last, walk the history (also ctrl+p, ctrl+n)",
    ),
    ("ctrl+t", "show reasoning in full, or as one line"),
    ("!command", "run a command in the workspace; the agent sees it with your next message"),
    ("@path, tab", "attach a file to the message; tab completes the path"),
    ("/compact [what]", "go on from a summary of the conversation, keeping what you name"),
    ("/undo", "put back the files the last turn changed; again for the turn before"),
    ("/diff [all]", "what changed since the last turn began, or since the session's first"),
    ("/new", "start a new session"),
    ("/tasks", "list the subagents working in the background"),
    ("/stop [n]", "stop them, or the nth; the agent gets their reports"),
    ("/reasoning", "the same as ctrl+t"),
    ("/quit, /exit", "quit"),
    ("/help", "this list"),
];

/// Runs an interactive session in the terminal until the user quits.
pub async fn run(mut options: Options) -> Result<()> {
    // The first session starts before the terminal changes, so a bad profile or directory is
    // reported the ordinary way.
    let agent = match &options.resume {
        Some(log) => Agent::resume(options.profile.clone(), agent_options(&options), log)?,
        None => start(&options)?,
    };
    // A session resumed from another directory goes on in its own, and so does /new after it.
    options.workspace = agent.workspace().to_path_buf();
    let resumed = options.resume.is_some();
    let (width, height) = terminal::size().context("reading the terminal's size")?;
    let terminal = Terminal::enter().context("setting up the terminal")?;
    let mut app = App::new(options, agent, Screen::new(io::stdout(), width, height));
    if resumed {
        app.resumed().await;
    }
    let result = app.run().await;
    let end = app.end();
    drop(terminal);
    result.and(end.map_err(Into::into))
}

fn start(options: &Options) -> Result<Agent> {
    Agent::start(options.profile.clone(), agent_options(options))
}

fn agent_options(options: &Options) -> AgentOptions {
    AgentOptions {
        workspace: options.workspace.clone(),
        session_root: options.session_root.clone(),
        headless: false,
        confirm: options.confirm.clone(),
        model_socket: None,
    }
}

/// What the agent's frontend sends the loop.
enum Message {
    Event(Event),
    Approve(Approval, oneshot::Sender<Decision>),
    Background(Background),
}

/// The agent's frontend: everything goes to the loop.
struct Channel(mpsc::UnboundedSender<Message>);

impl Frontend for Channel {
    fn event(&mut self, event: Event) {
        let _ = self.0.send(Message::Event(event));
    }

    async fn approve(&mut self, approval: &Approval) -> Decision {
        let (answer, answered) = oneshot::channel();
        if self.0.send(Message::Approve(approval.clone(), answer)).is_err() {
            return Decision::Deny { note: None };
        }
        answered.await.unwrap_or(Decision::Deny { note: None })
    }

    fn background(&mut self, task: Background) -> Result<(), Background> {
        match self.0.send(Message::Background(task)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::SendError(Message::Background(task))) => Err(task),
            Err(_) => unreachable!("what was sent was a task"),
        }
    }
}

type Run = Pin<Box<dyn Future<Output = (Agent, Result<Outcome>)>>>;

/// A /diff or /undo, while git works.
type Job = Pin<Box<dyn Future<Output = Done>>>;

enum Done {
    /// With `all`, the changes since the session's first turn began.
    Diff {
        all: bool,
        diff: Result<Option<(usize, String)>, String>,
    },
    Undo(Result<Option<Undone>, String>),
    /// A command typed after `!`: its output, and how it ended unless with 0.
    Command {
        command: String,
        output: String,
        status: Option<String>,
    },
}

/// A turn of the agent's, while it runs.
struct Turn {
    run: Run,
    interrupter: Interrupter,
    interrupted: bool,
    /// It is a `/compact`, rather than a reply to a message.
    compacting: bool,
    started: Instant,
    phase: Phase,
    /// The reply streaming in: its reasoning and its text.
    reasoning: String,
    reply: String,
    /// Tool calls under way, by id, with their descriptions.
    calls: Vec<(String, String)>,
    approval: Option<Pending>,
}

impl Turn {
    fn new(run: Run, interrupter: Interrupter) -> Self {
        Turn {
            run,
            interrupter,
            interrupted: false,
            compacting: false,
            started: Instant::now(),
            phase: Phase::Waiting,
            reasoning: String::new(),
            reply: String::new(),
            calls: Vec::new(),
            approval: None,
        }
    }
}

/// A command waiting for the user's approval.
struct Pending {
    approval: Approval,
    answer: oneshot::Sender<Decision>,
    shown: Instant,
}

/// A subagent working in the background, and what is known of it.
struct Task {
    run: Pin<Box<dyn Future<Output = Report>>>,
    /// The id of the `task` call that started it.
    id: String,
    label: String,
    interrupter: Interrupter,
    started: Instant,
    /// Its latest step, for /tasks.
    doing: String,
    /// The session it reports to. After /new, an earlier session's tasks report to no one.
    session: u64,
}

enum Phase {
    Waiting,
    Thinking,
    Writing,
    Running,
}

/// Kinds of things in the scrollback; a blank line separates different ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Info,
    User,
    Reply,
    Tools,
    Notice,
}

/// What to do when the running turn ends.
#[derive(PartialEq, Eq)]
enum After {
    Nothing,
    NewSession,
}

struct App<W: Write> {
    options: Options,
    screen: Screen<W>,
    editor: Editor,
    /// The agent, while the turn is the user's. During the agent's turn it is inside `turn`.
    agent: Option<Agent>,
    turn: Option<Turn>,
    log: PathBuf,
    /// The agent's working directory, which relative `@path` mentions start from.
    workspace: PathBuf,
    sender: mpsc::UnboundedSender<Message>,
    inbox: mpsc::UnboundedReceiver<Message>,
    /// Messages sent during the agent's turn, for when it ends.
    queued: Vec<String>,
    /// Ctrl+Enter stopped the turn: what is queued goes as soon as it has stopped, rather than
    /// back to the input box.
    send_now: bool,
    /// Subagents working in the background.
    tasks: Vec<Task>,
    /// Reports of background subagents that have ended, for the agent's next message.
    reports: Vec<Report>,
    /// Notes for the agent's next message, such as what /undo put back.
    notes: Vec<String>,
    /// The session's checkpoints, or why it has none.
    checkpoints: Result<Arc<Mutex<Checkpoints>>, String>,
    job: Option<Job>,
    /// The command typed after `!` that the job runs, and when it started.
    command: Option<(String, Instant)>,
    /// The files an `@path` could go on to, shown under the input box after a Tab.
    completions: Vec<String>,
    /// Counts the sessions started, so that a task reports only to its own.
    session: u64,
    /// The last request's numbers, for the status line.
    stats: Option<RequestStats>,
    /// Reasoning goes into the scrollback whole, rather than as one line.
    show_reasoning: bool,
    /// Lines for the scrollback, written at the next draw.
    finished: Vec<Line>,
    last_block: Option<Block>,
    after: After,
    /// Quitting, once the turn and the background subagents have stopped.
    quitting: bool,
    quit: bool,
    dirty: bool,
    urgent: bool,
    drawn: Instant,
}

impl<W: Write> App<W> {
    fn new(options: Options, agent: Agent, screen: Screen<W>) -> Self {
        let (sender, inbox) = mpsc::unbounded_channel();
        let checkpoints = checkpoints(&options, &agent);
        let mut app = App {
            options,
            screen,
            editor: Editor::new(),
            log: agent.session_log().to_path_buf(),
            workspace: agent.workspace().to_path_buf(),
            agent: Some(agent),
            turn: None,
            sender,
            inbox,
            queued: Vec::new(),
            send_now: false,
            tasks: Vec::new(),
            reports: Vec::new(),
            notes: Vec::new(),
            checkpoints,
            job: None,
            command: None,
            completions: Vec::new(),
            session: 0,
            stats: None,
            show_reasoning: false,
            finished: Vec::new(),
            last_block: None,
            after: After::Nothing,
            quitting: false,
            quit: false,
            dirty: true,
            urgent: true,
            drawn: Instant::now(),
        };
        app.header();
        app
    }

    async fn run(&mut self) -> Result<()> {
        let mut terminal = EventStream::new();
        let mut tick = interval(SPINNER_STEP);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Without these, a closed terminal or a kill would end eidra on the spot, leaving the
        // terminal as it was and the commands the agent started running.
        let mut hangup = signal(SignalKind::hangup()).context("handling SIGHUP")?;
        let mut terminate = signal(SignalKind::terminate()).context("handling SIGTERM")?;
        while !self.quit {
            if self.dirty && (self.urgent || self.drawn.elapsed() >= FRAME) {
                self.draw()?;
            }
            let frame = self.dirty.then(|| self.drawn + FRAME);
            tokio::select! {
                event = terminal.next() => match event {
                    Some(Ok(event)) => self.terminal_event(event),
                    Some(Err(e)) => return Err(e).context("reading the terminal"),
                    None => self.quit = true,
                },
                Some(message) = self.inbox.recv() => {
                    // Everything waiting, before the next draw: deltas arrive faster than frames.
                    self.message(message);
                    while let Ok(message) = self.inbox.try_recv() {
                        self.message(message);
                    }
                }
                (agent, outcome) = finished(&mut self.turn) => self.turn_ended(agent, outcome),
                (i, report) = next_report(&mut self.tasks), if !self.tasks.is_empty() => {
                    self.task_ended(i, report);
                }
                done = job_done(&mut self.job), if self.job.is_some() => self.job_ended(done),
                _ = tick.tick(), if self.turn.is_some() || self.command.is_some() => {
                    self.dirty = true;
                }
                () = sleep_until(frame.unwrap_or_else(Instant::now)), if frame.is_some() => {}
                _ = hangup.recv() => self.request_quit(),
                _ = terminate.recv() => self.request_quit(),
            }
        }
        Ok(())
    }

    /// Clears the live region and leaves the session's log in the scrollback, for the shell.
    fn end(&mut self) -> io::Result<()> {
        let log = format!("session log: {}", home_relative(&self.log));
        self.commit(Block::Info, vec![Line::styled(log, theme::DIM)]);
        let finished = mem::take(&mut self.finished);
        self.screen.draw(&finished, &[])
    }

    fn draw(&mut self) -> io::Result<()> {
        let live = self.live();
        let finished = mem::take(&mut self.finished);
        self.screen.draw(&finished, &live)?;
        self.dirty = false;
        self.urgent = false;
        self.drawn = Instant::now();
        Ok(())
    }

    fn header(&mut self) {
        let profile = &self.options.profile;
        let workspace = self.agent.as_ref().map_or(&*self.options.workspace, Agent::workspace);
        let first = format!(
            "eidra · {} · {} · {}",
            profile.model,
            profile.request.reasoning_effort,
            home_relative(workspace)
        );
        let session = self.agent.as_ref().map_or("", Agent::session_id);
        let second = format!("session {session} · /help for keys and commands");
        let third = self.instructions_line();
        let lines = vec![
            Line::styled(first, theme::BOLD),
            Line::styled(second, theme::DIM),
            Line::styled(third, theme::DIM),
        ];
        self.commit(Block::Info, lines);
    }

    /// Which AGENTS.md files the session's system prompt holds, so that the user can see theirs
    /// were read: the workspace's by its name, the user's own by its path.
    fn instructions_line(&self) -> String {
        let Some(agent) = &self.agent else {
            return String::new();
        };
        let workspace = agent.workspace();
        let files = agent.instructions();
        if files.is_empty() {
            let own = home_relative(&eidra_core::eidra_home(workspace).join("AGENTS.md"));
            return format!("instructions: none (no {own}, no AGENTS.md here)");
        }
        let shown: Vec<String> = files
            .iter()
            .map(|path| match path.strip_prefix(workspace) {
                Ok(inside) => inside.display().to_string(),
                Err(_) => home_relative(path),
            })
            .collect();
        format!("instructions: {}", shown.join(", "))
    }

    /// Shows the conversation a resumed session goes on from, and finds the checkpoints of its
    /// turns.
    async fn resumed(&mut self) {
        self.history();
        let Some(agent) = &self.agent else {
            return;
        };
        let (changed, workspace) = (agent.profile_changed(), agent.workspace().to_path_buf());
        let session = agent.session_id().to_owned();
        if changed {
            self.notice(
                "the profile has changed since this session began; it keeps its system prompt and \
                 tools, and the rest of the profile is the new one",
            );
        }
        if let Some(root) = &self.options.checkpoints {
            let found = Checkpoints::resume(root, &workspace, &session).await;
            self.checkpoints = found.map(|checkpoints| Arc::new(Mutex::new(checkpoints)));
        }
    }

    /// The last conversation of the session's log, as it was shown while it happened.
    fn history(&mut self) {
        let log = match Log::read(&self.log) {
            Ok(log) => log,
            Err(e) => return self.error(&format!("reading the session's log: {}", causes(&e))),
        };
        let start = log.starts.last().copied().unwrap_or(0);
        let messages = &log.messages[start..];
        let skipped = messages.len().saturating_sub(HISTORY);
        let mut before = Vec::new();
        if start > 0 {
            before.push(format!("it was compacted {} times", log.starts.len() - 1));
        }
        if skipped > 0 {
            before.push(format!("{skipped} earlier messages are not shown"));
        }
        let width = self.width();
        for (message, stats) in &messages[skipped..] {
            match message {
                Logged::System { .. } => {}
                Logged::User { content } => {
                    let text = text_of(content);
                    let lines =
                        hanging(Line::styled("› ", theme::USER), &text, Style::PLAIN, width);
                    self.commit(Block::User, lines);
                }
                Logged::Assistant(reply) => {
                    let reasoning = reply.reasoning_content.as_deref().unwrap_or_default();
                    let content = reply.content.as_deref().unwrap_or_default();
                    self.commit_reply(reasoning, content, stats.as_ref());
                    for call in &reply.tool_calls {
                        let what = describe_call(&call.function.name, &call.function.arguments);
                        self.commit(Block::Tools, call_lines(None, &what, width));
                    }
                    if stats.is_some() {
                        self.stats.clone_from(stats);
                    }
                }
                Logged::Tool { content, .. } => {
                    let text = text_of(content);
                    let lines = result_lines(None, &text, text.starts_with("Error"), width);
                    self.commit(Block::Tools, lines);
                }
            }
        }
        let mut info =
            format!("resumed: the conversation goes on from here ({} messages", messages.len());
        for part in before {
            info.push_str(&format!("; {part}"));
        }
        self.info(&(info + ")"));
    }

    // The scrollback.

    /// Adds lines to the scrollback, after a blank line if they start something new.
    fn commit(&mut self, block: Block, lines: Vec<Line>) {
        if lines.is_empty() {
            return;
        }
        let continues = matches!(
            (self.last_block, block),
            (Some(Block::Tools), Block::Tools) | (Some(Block::Notice), Block::Notice)
        );
        if self.last_block.is_some() && !continues {
            self.finished.push(Line::new());
        }
        self.finished.extend(lines);
        self.last_block = Some(block);
        self.dirty = true;
    }

    fn notice(&mut self, text: &str) {
        let lines = hanging(Line::styled("! ", theme::NOTICE), text, theme::NOTICE, self.width());
        self.commit(Block::Notice, lines);
    }

    fn error(&mut self, text: &str) {
        let lines = hanging(Line::styled("✗ ", theme::ERROR), text, theme::ERROR, self.width());
        self.commit(Block::Notice, lines);
    }

    fn info(&mut self, text: &str) {
        self.commit(Block::Info, style::lines(text, theme::DIM));
    }

    /// A reply: its reasoning, whole or as one line, then its text as markdown.
    fn commit_reply(&mut self, reasoning: &str, reply: &str, stats: Option<&RequestStats>) {
        let mut lines = Vec::new();
        let reasoning = reasoning.trim();
        if !reasoning.is_empty() {
            if self.show_reasoning {
                lines.extend(style::lines(reasoning, theme::REASONING));
            } else {
                let tokens = stats
                    .and_then(|s| s.reasoning_tokens)
                    .map(|n| format!(" for {} tokens", thousands(n)))
                    .unwrap_or_default();
                lines.push(Line::styled(format!("∴ thought{tokens}"), theme::REASONING));
            }
        }
        let reply = reply.trim();
        if reply.is_empty() {
            // Reasoning alone leads into the tool calls after it, as one step.
            self.commit(Block::Tools, lines);
            return;
        }
        if self.show_reasoning && !lines.is_empty() {
            lines.push(Line::new());
        }
        lines.extend(markdown::render(reply, self.width()));
        self.commit(Block::Reply, lines);
    }

    fn width(&self) -> usize {
        self.screen.width()
    }

    // What the agent sends.

    fn message(&mut self, message: Message) {
        match message {
            Message::Event(event) => self.event(event),
            Message::Approve(approval, answer) => {
                // Without a turn the agent is not waiting for it any more.
                if let Some(turn) = &mut self.turn {
                    turn.approval = Some(Pending { approval, answer, shown: Instant::now() });
                    self.urgent = true;
                }
            }
            Message::Background(task) => self.start_task(task),
        }
        self.dirty = true;
    }

    fn event(&mut self, event: Event) {
        // A subagent's steps, whether it runs within the turn or in the background.
        if let Event::Subagent { task, label, event } = event {
            self.subagent_event(&task, &label, *event);
            return;
        }
        let Some(turn) = &mut self.turn else {
            return;
        };
        match event {
            Event::Request { .. } => {
                turn.phase = Phase::Waiting;
            }
            Event::Reasoning { text } => {
                turn.reasoning.push_str(&text);
                turn.phase = Phase::Thinking;
            }
            Event::Content { text } => {
                turn.reply.push_str(&text);
                turn.phase = Phase::Writing;
            }
            Event::Reply { stats } => {
                let reasoning = mem::take(&mut turn.reasoning);
                let reply = mem::take(&mut turn.reply);
                self.commit_reply(&reasoning, &reply, Some(&stats));
                self.stats = Some(stats);
            }
            Event::ToolCall { id, name, arguments } => {
                let description = describe_call(&name, &arguments);
                turn.calls.push((id, description.clone()));
                turn.phase = Phase::Running;
                // A todo call shows as its list, from the Todo event.
                if name != "todo" {
                    self.commit(Block::Tools, call_lines(None, &description, self.width()));
                }
            }
            Event::ToolResult { id, name, output, is_error } => {
                turn.calls.retain(|(call, _)| *call != id);
                // A task started in the background shows as its subagent's steps: the result is
                // for the model. (The subagent reaches the loop before its call's result does.)
                let started = name == "task" && self.tasks.iter().any(|task| task.id == id);
                if (name != "todo" || is_error) && !(started && !is_error) {
                    let lines = result_lines(None, &output, is_error, self.width());
                    self.commit(Block::Tools, lines);
                }
            }
            Event::Todo { list } => {
                let width = self.width();
                let mut lines = call_lines(None, "todo", width);
                for row in markdown::render(&list, width.saturating_sub(4)) {
                    lines.push(Line::plain("    ").with_line(row));
                }
                self.commit(Block::Tools, lines);
            }
            Event::Notice { text } => self.notice(&text),
            Event::Subagent { .. } => {}
        }
    }

    /// A subagent's tool calls, results and notices, under its task's label. Its `finish` is left
    /// out: the report that ends the task says it.
    fn subagent_event(&mut self, id: &str, label: &str, event: Event) {
        let width = self.width();
        let lines = match event {
            Event::ToolCall { name, .. } | Event::ToolResult { name, .. } if name == "finish" => {
                return;
            }
            Event::ToolCall { name, arguments, .. } => {
                let description = describe_call(&name, &arguments);
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                    task.doing.clone_from(&description);
                }
                call_lines(Some(label), &description, width)
            }
            Event::ToolResult { output, is_error, .. } => {
                result_lines(Some(label), &output, is_error, width)
            }
            Event::Notice { text } => {
                let head =
                    Line::styled(format!("  [{label}] "), theme::DIM).with("! ", theme::NOTICE);
                hanging(head, &text, theme::NOTICE, width)
            }
            _ => return,
        };
        self.commit(Block::Tools, lines);
    }

    /// Runs a subagent the agent started in the background, alongside everything else.
    fn start_task(&mut self, task: Background) {
        let sender = self.sender.clone();
        let (id, label, interrupter) = (task.task.clone(), task.label.clone(), task.interrupter());
        let run = Box::pin(task.run(move |event| {
            let _ = sender.send(Message::Event(event));
        }));
        let (started, session) = (Instant::now(), self.session);
        let doing = String::new();
        self.tasks.push(Task { run, id, label, interrupter, started, doing, session });
    }

    /// A background subagent ended. Its report goes to the agent at once if the agent is free,
    /// else when its turn ends, or after an interrupt with the user's next message.
    fn task_ended(&mut self, i: usize, report: Report) {
        let task = self.tasks.remove(i);
        // What it did before it ended comes first.
        while let Ok(message) = self.inbox.try_recv() {
            self.message(message);
        }
        let width = self.width();
        let took = duration(task.started.elapsed().as_secs_f64());
        let what = format!("background task \"{}\" ended after {took}", task.label);
        let mut lines = hanging(Line::styled("◆ ", theme::TOOL), &what, Style::PLAIN, width);
        lines.extend(result_lines(None, &report.text, report.is_error, width));
        self.commit(Block::Tools, lines);
        if task.session == self.session && !self.quitting {
            self.reports.push(report);
            if !self.busy() {
                self.send(String::new());
            }
        }
        self.check_quit();
    }

    fn turn_ended(&mut self, agent: Agent, outcome: Result<Outcome>) {
        // What the agent sent before it returned comes first.
        while let Ok(message) = self.inbox.try_recv() {
            self.message(message);
        }
        let Some(turn) = self.turn.take() else {
            return;
        };
        let send_now = mem::take(&mut self.send_now);
        // The busy line goes, even when nothing else changes: the snapshot after a turn can end
        // it a while after its last event was drawn.
        self.dirty = true;
        self.agent = Some(agent);
        // A reply cut short never reached the conversation, but show what there was of it.
        let partial = !turn.reasoning.trim().is_empty() || !turn.reply.trim().is_empty();
        if partial {
            self.commit_reply(&turn.reasoning, &turn.reply, None);
        }
        let limit = self.options.profile.context.limit;
        let interrupted = matches!(outcome, Ok(Outcome::Interrupted));
        let normal = match outcome {
            Ok(Outcome::Replied) => true,
            Ok(Outcome::Finished(summary)) => {
                let lines = markdown::render(&summary, self.width());
                self.commit(Block::Reply, lines);
                true
            }
            Ok(Outcome::Interrupted) => {
                self.notice(if partial {
                    "interrupted; the unfinished reply is not part of the conversation"
                } else {
                    "interrupted"
                });
                false
            }
            Ok(Outcome::RequestLimit) => {
                let text = format!(
                    "stopped after {MAX_REQUESTS} requests in one turn; send a message to go on"
                );
                self.notice(&text);
                false
            }
            Ok(Outcome::ContextLimit(tokens)) => {
                let text = format!(
                    "the conversation reached {} tokens, past the profile's limit of {}; /new \
                     starts a new session",
                    thousands(tokens),
                    thousands(limit)
                );
                self.notice(&text);
                false
            }
            Err(e) => {
                self.error(&causes(&e));
                false
            }
        };
        // The last reply's numbers were those of the conversation a /compact replaced.
        if turn.compacting && self.agent.as_ref().is_some_and(|agent| agent.context_used() == 0) {
            self.stats = None;
        }
        if mem::replace(&mut self.after, After::Nothing) == After::NewSession {
            self.new_session();
        }
        self.check_quit();
        if self.quitting {
            return;
        }
        // Stopped by Ctrl+Enter, the turn hands over to what was queued, as a reply would.
        let go_on = normal || (send_now && interrupted);
        if go_on && self.job.is_none() {
            // What was queued, after the reports that came in meanwhile.
            let queued = mem::take(&mut self.queued).join("\n\n");
            self.send(queued);
        } else if !go_on {
            self.return_queued();
        }
        // With a /diff, /undo or ! command still running, what was queued waits for it.
    }

    /// After an interrupt or an error, what was queued goes back to the input box, to send or
    /// change. Reports wait for the next message.
    fn return_queued(&mut self) {
        let queued = mem::take(&mut self.queued).join("\n\n");
        if queued.is_empty() {
            return;
        }
        let draft = self.editor.text().trim();
        let text = if draft.is_empty() { queued } else { format!("{queued}\n\n{draft}") };
        self.editor.set(text);
    }

    // What the user does.

    fn terminal_event(&mut self, event: TerminalEvent) {
        match event {
            TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            TerminalEvent::Paste(text) if !self.approving() => self.editor.insert(&text),
            TerminalEvent::Resize(width, height) => self.screen.resize(width, height),
            _ => return,
        }
        self.dirty = true;
        self.urgent = true;
    }

    fn key(&mut self, key: KeyEvent) {
        self.completions.clear();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let answerable = self.turn.as_ref().and_then(|turn| turn.approval.as_ref());
        if answerable.is_some_and(|pending| pending.shown.elapsed() >= APPROVAL_GRACE)
            && !ctrl
            && !alt
        {
            let decision = match key.code {
                KeyCode::Char('y' | 'Y') => Some(Decision::Once),
                KeyCode::Char('a' | 'A') => Some(Decision::Session),
                KeyCode::Char('n' | 'N') => Some(Decision::Deny { note: None }),
                _ => None,
            };
            if let Some(decision) = decision {
                self.answer(decision);
                return;
            }
        }
        match key.code {
            KeyCode::Esc => self.interrupt(),
            KeyCode::Char('c') if ctrl => {
                if self.quitting {
                    // Pressed again while things are stopping, it quits regardless.
                    self.quit = true;
                } else if let Some(turn) = &self.turn {
                    if turn.interrupted {
                        self.quit = true;
                    } else {
                        self.interrupt();
                    }
                } else if self.command.is_some() {
                    self.interrupt();
                } else if !self.editor.is_empty() {
                    self.editor.clear();
                } else {
                    self.request_quit();
                }
            }
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => self.request_quit(),
            KeyCode::Char('t') if ctrl => self.toggle_reasoning(),
            // While a command waits for approval, the input box takes nothing else.
            _ if self.approving() => {}
            KeyCode::Char('d') if ctrl => {
                self.editor.key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
            }
            KeyCode::Char('j') if ctrl => self.editor.insert("\n"),
            KeyCode::Tab if !ctrl && !alt && self.complete() => {}
            KeyCode::Enter if ctrl => self.submit_now(),
            KeyCode::Enter if shift || alt => self.editor.insert("\n"),
            KeyCode::Enter => self.submit(),
            _ => {
                self.editor.key(key);
            }
        }
    }

    /// Completes the `@path` before the caret, if there is one there.
    fn complete(&mut self) -> bool {
        let before = self.editor.before_caret();
        let start = before
            .rfind(char::is_whitespace)
            .map_or(0, |i| i + before[i..].chars().next().map_or(1, char::len_utf8));
        let word = &before[start..];
        let Some((completed, names)) = attach::complete(word, &self.workspace) else {
            return false;
        };
        let len = word.len();
        self.editor.replace_before_caret(len, &completed);
        self.completions = names;
        true
    }

    fn approving(&self) -> bool {
        self.turn.as_ref().is_some_and(|turn| turn.approval.is_some())
    }

    fn answer(&mut self, decision: Decision) {
        if let Some(pending) = self.turn.as_mut().and_then(|turn| turn.approval.take()) {
            let _ = pending.answer.send(decision);
        }
    }

    fn interrupt(&mut self) {
        // A command typed after `!` stops with its job: dropping it stops its process group.
        if let Some((command, _)) = self.command.take() {
            self.job = None;
            self.dirty = true;
            self.notice(&format!("stopped `{}`", clip(&command, 60)));
            // What waited for it goes back to the input box, as after an interrupted turn: the
            // user stopped things, so nothing starts by itself.
            if self.turn.is_none() {
                self.return_queued();
            }
            self.check_quit();
        }
        if let Some(turn) = &mut self.turn
            && !turn.interrupted
        {
            turn.interrupter.interrupt();
            turn.interrupted = true;
            // The agent stops waiting for an answer, so the question goes.
            turn.approval = None;
        }
    }

    /// Stops the turn and the background subagents, and quits once they have stopped.
    fn request_quit(&mut self) {
        self.quitting = true;
        self.interrupt();
        for task in &self.tasks {
            task.interrupter.interrupt();
        }
        self.check_quit();
    }

    fn check_quit(&mut self) {
        // An undo stopped halfway would leave the workspace neither as it was nor as it is.
        if self.quitting && self.turn.is_none() && self.tasks.is_empty() && self.job.is_none() {
            self.quit = true;
        }
    }

    /// The agent's turn or a /diff or /undo is under way: a message waits for it.
    fn busy(&self) -> bool {
        self.turn.is_some() || self.job.is_some()
    }

    fn toggle_reasoning(&mut self) {
        self.show_reasoning = !self.show_reasoning;
        self.info(if self.show_reasoning {
            "reasoning: shown in full from the next reply"
        } else {
            "reasoning: one line per reply"
        });
    }

    fn submit(&mut self) {
        let text = self.editor.submit();
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(command) = command(text) {
            let rest = text[command.len()..].trim().to_owned();
            self.command(command, &rest);
        } else if let Some(command) = text.strip_prefix('!') {
            self.run_command(command.trim());
        } else if self.busy() {
            self.queued.push(text.to_owned());
        } else {
            self.send(text.to_owned());
        }
    }

    /// Ctrl+Enter: during the agent's turn, stops it, and sends the message after any queued
    /// before it as soon as the turn has stopped. Otherwise, and for commands, the same as Enter.
    fn submit_now(&mut self) {
        let typed = self.editor.text().trim();
        let commanded = command(typed).is_some() || typed.starts_with('!');
        self.submit();
        if self.turn.is_some() && !commanded && !self.queued.is_empty() {
            self.send_now = true;
            self.interrupt();
        }
    }

    fn command(&mut self, command: &str, rest: &str) {
        match command {
            "/help" => {
                let lines = HELP
                    .iter()
                    .map(|(key, what)| Line::plain(format!("  {key:<16}")).with(what, theme::DIM))
                    .collect();
                self.commit(Block::Info, lines);
            }
            "/quit" | "/exit" => self.request_quit(),
            "/new" if self.job.is_some() => {
                self.notice("a /diff, /undo or ! command is still running: /new once it ends");
            }
            "/new" if self.turn.is_some() => {
                self.after = After::NewSession;
                self.interrupt();
            }
            "/new" => self.new_session(),
            "/tasks" => self.list_tasks(),
            "/stop" => self.stop_tasks(rest),
            "/compact" => self.compact_conversation(rest),
            "/undo" => self.undo(),
            "/diff" => self.diff(rest),
            "/reasoning" => self.toggle_reasoning(),
            _ => self.notice(&format!("{command} is not a command; /help lists them")),
        }
    }

    fn new_session(&mut self) {
        match start(&self.options) {
            Ok(agent) => {
                // The old agent goes, and with it any background jobs it started. Its background
                // subagents are stopped, and report to no one.
                for task in &self.tasks {
                    task.interrupter.interrupt();
                }
                self.session += 1;
                self.reports.clear();
                self.notes.clear();
                self.checkpoints = checkpoints(&self.options, &agent);
                self.log = agent.session_log().to_path_buf();
                self.agent = Some(agent);
                self.stats = None;
                self.info(&format!("new session · log {}", home_relative(&self.log)));
                // AGENTS.md is read again for it, and may have changed.
                let instructions = self.instructions_line();
                self.info(&instructions);
            }
            Err(e) => self.error(&format!("no new session: {}", causes(&e))),
        }
    }

    fn list_tasks(&mut self) {
        if self.tasks.is_empty() {
            self.info("no subagents are working in the background");
            return;
        }
        let lines = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, task)| {
                let doing = if task.doing.is_empty() { "starting" } else { &task.doing };
                let took = duration(task.started.elapsed().as_secs_f64());
                Line::plain(format!("  {}  {}", i + 1, task.label))
                    .with(&format!(" · {took} · {}", clip(doing, 60)), theme::DIM)
            })
            .collect();
        self.commit(Block::Info, lines);
    }

    /// Stops the background subagents, or the one numbered as /tasks lists them.
    fn stop_tasks(&mut self, which: &str) {
        let chosen: Vec<usize> = match which.parse::<usize>() {
            _ if which.is_empty() => (0..self.tasks.len()).collect(),
            Ok(n) if (1..=self.tasks.len()).contains(&n) => vec![n - 1],
            _ => {
                self.notice(&format!("there is no background task {which}; /tasks lists them"));
                return;
            }
        };
        if chosen.is_empty() {
            self.info("no subagents are working in the background");
        }
        for i in chosen {
            self.tasks[i].interrupter.interrupt();
            let label = self.tasks[i].label.clone();
            self.info(&format!("stopping \"{label}\"; the agent gets its report"));
        }
    }

    /// Summarises the conversation and goes on from the summary, as the agent does by itself past
    /// `[context] compact_at`, in a turn of its own that Esc stops. `focus` is what to keep.
    fn compact_conversation(&mut self, focus: &str) {
        if self.turn.is_some() {
            self.notice(
                "the agent is working: /compact once its turn ends, or press esc to stop it",
            );
            return;
        }
        let Some(mut agent) = self.agent.take() else {
            return;
        };
        let interrupter = agent.interrupter();
        let mut channel = Channel(self.sender.clone());
        let focus = focus.to_owned();
        let run: Run = Box::pin(async move {
            // Compacted, or left as it was after an empty summary: either way the turn is the
            // user's again, as after a reply.
            let outcome = agent.compact_now(&mut channel, &focus).await;
            (agent, outcome.map(|stopped| stopped.unwrap_or(Outcome::Replied)))
        });
        let mut turn = Turn::new(run, interrupter);
        turn.compacting = true;
        self.turn = Some(turn);
    }

    /// Puts back the files the last turn that changed any changed, through the checkpoints.
    fn undo(&mut self) {
        if self.turn.is_some() {
            self.notice("the agent is working: /undo once its turn ends, or press esc to stop it");
            return;
        }
        if let Some(checkpoints) = self.checkpoints_for_a_job() {
            self.job =
                Some(Box::pin(async move { Done::Undo(checkpoints.lock().await.undo().await) }));
        }
    }

    fn diff(&mut self, rest: &str) {
        let all = match rest {
            "" => false,
            "all" => true,
            _ => {
                self.notice("/diff shows what changed since the last turn began; /diff all, since the session's first");
                return;
            }
        };
        if let Some(checkpoints) = self.checkpoints_for_a_job() {
            self.job = Some(Box::pin(async move {
                Done::Diff { all, diff: checkpoints.lock().await.diff(all).await }
            }));
        }
    }

    /// Runs a command typed after `!` in the workspace. Its output goes into the scrollback, and
    /// to the agent with the next message.
    fn run_command(&mut self, command: &str) {
        if command.is_empty() {
            self.notice("type a command after !, such as !git status");
            return;
        }
        if self.job.is_some() {
            self.notice("the last /diff, /undo or command has not finished yet");
            return;
        }
        let width = self.width();
        let lines = hanging(Line::styled("! ", theme::USER), command, theme::BOLD, width);
        self.commit(Block::User, lines);
        let (workspace, command) = (self.workspace.clone(), command.to_owned());
        let secret = self.options.profile.api_key_env.clone();
        self.command = Some((command.clone(), Instant::now()));
        self.job = Some(Box::pin(async move {
            let (output, status) =
                eidra_core::run_command(&workspace, &command, COMMAND_TIMEOUT, secret.as_deref())
                    .await;
            Done::Command { command, output, status }
        }));
    }

    fn checkpoints_for_a_job(&mut self) -> Option<Arc<Mutex<Checkpoints>>> {
        if self.job.is_some() {
            self.notice("the last /diff or /undo has not finished yet");
            return None;
        }
        match &self.checkpoints {
            Ok(checkpoints) => Some(Arc::clone(checkpoints)),
            Err(why) => {
                let why = format!("checkpoints are off: {why}");
                self.notice(&why);
                None
            }
        }
    }

    fn job_ended(&mut self, done: Done) {
        self.job = None;
        self.dirty = true;
        match done {
            Done::Diff { diff: Ok(None), .. } => self.info("no turn has run in this session yet"),
            Done::Diff { all, diff: Ok(Some((turn, diff))) } => {
                let since = if all { "the session's first turn" } else { "the last turn" };
                if diff.trim().is_empty() {
                    self.info(&format!("nothing has changed since {since} (turn {turn}) began"));
                } else {
                    let head = format!("changes since {since} (turn {turn}) began");
                    let mut lines = vec![Line::styled(head, theme::BOLD)];
                    lines.extend(diff_lines(&diff, DIFF_LINES));
                    self.commit(Block::Info, lines);
                }
            }
            Done::Undo(Ok(None)) => self.info("no turn of this session is left that changed files"),
            Done::Undo(Ok(Some(undone))) => self.undone(undone),
            Done::Diff { diff: Err(why), .. } => self.error(&format!("/diff: {why}")),
            Done::Undo(Err(why)) => self.error(&format!("/undo: {why}")),
            Done::Command { command, output, status } => {
                self.command_ended(command, output, status)
            }
        }
        self.after_job();
    }

    fn after_job(&mut self) {
        self.dirty = true;
        self.check_quit();
        if !self.quitting && self.turn.is_none() {
            // Messages and reports that waited for it.
            let queued = mem::take(&mut self.queued).join("\n\n");
            self.send(queued);
        }
    }

    /// Shows what a `!` command printed, its start and end if it is long, and keeps all of it, up
    /// to the profile's limit for a tool's output, for the agent's next message.
    fn command_ended(&mut self, command: String, output: String, status: Option<String>) {
        self.command = None;
        let rows: Vec<&str> = output.lines().collect();
        let mut lines: Vec<Line> = Vec::new();
        let show = |line: &str| Line::styled(format!("  {}", style::expand_tabs(line)), theme::DIM);
        if rows.len() > COMMAND_HEAD + COMMAND_TAIL + 1 {
            lines.extend(rows[..COMMAND_HEAD].iter().map(|line| show(line)));
            let hidden = rows.len() - COMMAND_HEAD - COMMAND_TAIL;
            lines.push(Line::styled(format!("  … {hidden} lines …"), theme::DIM));
            lines.extend(rows[rows.len() - COMMAND_TAIL..].iter().map(|line| show(line)));
        } else {
            lines.extend(rows.iter().map(|line| show(line)));
        }
        let ended = match &status {
            None => "succeeded".to_owned(),
            Some(how) => format!("failed ({how})"),
        };
        let style = if status.is_some() { theme::ERROR } else { theme::DIM };
        lines.push(Line::styled(format!("  ⎿ {ended}"), style));
        self.commit(Block::Tools, lines);
        let profile = &self.options.profile;
        let output = if output.is_empty() { "(no output)".to_owned() } else { output };
        let output = eidra_core::truncate_middle(&output, profile.tools.output_limit, "");
        let note = profile
            .prompts
            .user_command
            .replace("{command}", &command)
            .replace("{status}", &ended)
            .replace("{output}", &output);
        self.notes.push(note);
    }

    /// Shows what an undo did, and keeps a note of it for the agent's next message: without one,
    /// the model believes its changes are still there.
    fn undone(&mut self, undone: Undone) {
        let width = self.width();
        let head = format!("undid turn {} · {}", undone.turn, undone.label);
        let mut lines = hanging(Line::styled("↶ ", theme::NOTICE), &head, Style::PLAIN, width);
        let groups = [
            ("put back", &undone.restored),
            ("removed, since the turn made them", &undone.removed),
            ("left as they are, since they changed after the turn", &undone.kept),
        ];
        for (what, files) in groups {
            if !files.is_empty() {
                let text = format!("{what}: {}", files.join(", "));
                lines.extend(hanging(Line::styled("  ⎿ ", theme::DIM), &text, theme::DIM, width));
            }
        }
        self.commit(Block::Info, lines);
        let list = |files: &[String]| {
            if files.is_empty() { "none".to_owned() } else { files.join(", ") }
        };
        let note = self
            .options
            .profile
            .prompts
            .undone
            .replace("{turn}", &undone.label)
            .replace("{restored}", &list(&undone.restored))
            .replace("{removed}", &list(&undone.removed))
            .replace("{kept}", &list(&undone.kept));
        self.notes.push(note);
    }

    /// Sends a message, after the reports of background subagents that have ended, and starts
    /// the agent's turn.
    fn send(&mut self, text: String) {
        if text.is_empty() && self.reports.is_empty() {
            return;
        }
        let Some(mut agent) = self.agent.take() else {
            return;
        };
        let label =
            if text.is_empty() { "(reports of background tasks)".into() } else { text.clone() };
        let mut images = Vec::new();
        let mut files = Vec::new();
        if !text.is_empty() {
            let user = Line::styled("› ", theme::USER);
            let mut lines = hanging(user, &text, Style::PLAIN, self.width());
            let mut attached = Vec::new();
            for attachment in attach::attachments(&text, &self.workspace) {
                attached.push(match attachment.image {
                    Ok(image) => {
                        let what = format!("attached {}: {}", attachment.name, image.describe());
                        images.push(image);
                        Ok(what)
                    }
                    Err(why) => Err(format!("not attached {}: {why}", attachment.name)),
                });
            }
            for name in attach::mentioned_files(&text, &self.workspace) {
                attached.push(match agent.attach_file(&name) {
                    Ok(file) => {
                        files.push(file);
                        let bytes = std::fs::read(self.workspace.join(&name)).unwrap_or_default();
                        let lines = bytecount_lines(&bytes);
                        let plural = if lines == 1 { "" } else { "s" };
                        Ok(format!("attached {name}: {lines} line{plural}"))
                    }
                    Err(why) => Err(format!("not attached {name}: {why}")),
                });
            }
            for what in attached {
                let (what, style) = match what {
                    Ok(what) => (what, theme::DIM),
                    Err(why) => (why, theme::ERROR),
                };
                lines.extend(hanging(Line::styled("  ⎿ ", theme::DIM), &what, style, self.width()));
            }
            self.commit(Block::User, lines);
        }
        let mut parts = self.notes.clone();
        parts.extend(self.reports.iter().map(|r| agent.report_message(r)));
        if !text.is_empty() {
            parts.push(text);
        }
        // Files the message mentions follow it, so that it reads as the user wrote it.
        parts.extend(files);
        if let Err(e) = agent.push_user_with(&parts.join("\n\n"), images) {
            self.agent = Some(agent);
            self.error(&causes(&e));
            return;
        }
        self.reports.clear();
        self.notes.clear();
        let interrupter = agent.interrupter();
        let mut channel = Channel(self.sender.clone());
        let checkpoints = self.checkpoints.clone().ok();
        let run: Run = Box::pin(async move {
            // The workspace before the turn and after it, for /undo and /diff.
            if let Some(checkpoints) = &checkpoints
                && let Err(why) = checkpoints.lock().await.begin(&label).await
            {
                channel.event(Event::Notice { text: format!("checkpoints are off: {why}") });
            }
            let outcome = agent.run(&mut channel, MAX_REQUESTS).await;
            if let Some(checkpoints) = &checkpoints
                && let Err(why) = checkpoints.lock().await.end().await
            {
                channel.event(Event::Notice { text: format!("checkpoints are off: {why}") });
            }
            (agent, outcome)
        });
        self.turn = Some(Turn::new(run, interrupter));
    }

    // The live region.

    /// The live region, top to bottom: the streaming tail, what the agent is doing, queued
    /// messages, a command waiting for approval, the input box under a rule, and the status line.
    fn live(&mut self) -> Vec<Line> {
        let width = self.screen.live_width();
        let height = self.screen.live_height();
        let mut bottom = Vec::new();
        if let Some(turn) = &self.turn {
            bottom.push(Line::new());
            bottom.push(busy_line(turn));
        }
        if let Some((command, started)) = &self.command {
            let elapsed = started.elapsed();
            let frame = SPINNER[(elapsed.as_millis() / SPINNER_STEP.as_millis()) as usize % 10];
            let took = duration(elapsed.as_secs_f64());
            bottom.push(Line::new());
            bottom.push(
                Line::styled(format!("{frame} "), theme::SPINNER)
                    .with(&format!("running {}", clip(command, 50)), Style::PLAIN)
                    .with(&format!(" · {took} · esc to stop"), theme::DIM),
            );
        }
        for (i, message) in self.queued.iter().enumerate() {
            if i == 3 {
                bottom.push(Line::styled(
                    format!("  and {} more", self.queued.len() - 3),
                    theme::DIM,
                ));
                break;
            }
            let first = message.lines().next().unwrap_or_default();
            let line = format!("queued › {}", clip(first, width.saturating_sub(12)));
            bottom.push(Line::styled(line, theme::DIM));
        }
        if let Some(pending) = self.turn.as_ref().and_then(|t| t.approval.as_ref()) {
            bottom.extend(approval_lines(&pending.approval, width));
        }
        bottom.push(Line::styled("─".repeat(width), theme::DIM));
        let placeholder = if self.turn.is_some() {
            "Queue message"
        } else {
            "Message eidra · /help for keys and commands"
        };
        let approving = self.approving();
        bottom.extend(self.editor.render(width, (height / 2).max(1), !approving, placeholder));
        if !self.completions.is_empty() {
            let names = clip(&self.completions.join("  "), width.saturating_sub(3));
            bottom.push(Line::styled(format!("  {names}"), theme::DIM));
        }
        bottom.push(self.status_line(width));

        let bottom = wrap_all(&bottom, width);
        let mut live = self.tail(width, height.saturating_sub(bottom.len()));
        live.extend(bottom);
        live
    }

    /// The end of the reply streaming in, or of its reasoning before the reply starts.
    fn tail(&self, width: usize, room: usize) -> Vec<Line> {
        let Some(turn) = &self.turn else {
            return Vec::new();
        };
        let height = self.screen.live_height();
        let mut rows = if !turn.reply.trim().is_empty() {
            let rows = markdown::render(turn.reply.trim_start(), width);
            let keep = rows.len().min(room).min((height / 2).max(3));
            rows[rows.len() - keep..].to_vec()
        } else {
            // Only the last lines are wrapped, however long the reasoning grows.
            let keep = room.min((height / 4).max(3));
            let mut rows: Vec<Line> = Vec::new();
            for line in turn.reasoning.trim().lines().rev() {
                if rows.len() >= keep {
                    break;
                }
                let line = Line::styled(style::expand_tabs(line), theme::REASONING);
                let mut wrapped = wrap(&line, width);
                wrapped.append(&mut rows);
                rows = wrapped;
            }
            rows.split_off(rows.len().saturating_sub(keep))
        };
        rows.truncate(room);
        rows
    }

    fn status_line(&self, width: usize) -> Line {
        let profile = &self.options.profile;
        let mut parts = vec![profile.model.clone(), profile.request.reasoning_effort.clone()];
        let plural = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        if !self.tasks.is_empty() {
            parts.push(format!("{} running", plural(self.tasks.len(), "task")));
        }
        if !self.reports.is_empty() {
            parts.push(format!("{} waiting", plural(self.reports.len(), "report")));
        }
        let limit = compact(profile.context.limit as f64);
        match &self.stats {
            None => parts.push(format!("0/{limit}")),
            Some(stats) => {
                let prompt = stats.prompt_tokens.unwrap_or(0);
                let cached = stats.cached_tokens.unwrap_or(0);
                let used = prompt + stats.completion_tokens.unwrap_or(0);
                parts.push(format!("{}/{limit}", compact(used as f64)));
                if let Some(share) = (100 * cached).checked_div(prompt) {
                    parts.push(format!("cache {share}%"));
                }
                if let Some(ms) = stats.prefill_ms.filter(|&ms| ms > 0.0) {
                    let computed = prompt.saturating_sub(cached) as f64;
                    parts.push(format!("prefill {} tok/s", compact(computed * 1000.0 / ms)));
                }
                if let Some(rate) = stats.decode_per_second {
                    parts.push(format!("decode {rate:.0} tok/s"));
                }
            }
        }
        Line::styled(clip(&parts.join(" · "), width.saturating_sub(1)), theme::DIM)
    }
}

/// The running turn's future, or one that never ends.
async fn finished(turn: &mut Option<Turn>) -> (Agent, Result<Outcome>) {
    match turn {
        Some(turn) => turn.run.as_mut().await,
        None => std::future::pending().await,
    }
}

/// Lines in a file's bytes, a last line without a line break among them.
fn bytecount_lines(bytes: &[u8]) -> usize {
    let breaks = bytes.iter().filter(|&&b| b == b'\n').count();
    breaks + usize::from(bytes.last().is_some_and(|&b| b != b'\n'))
}

async fn job_done(job: &mut Option<Job>) -> Done {
    match job {
        Some(job) => job.as_mut().await,
        None => std::future::pending().await,
    }
}

/// The session's checkpoints, if the options have a place for them and the workspace allows them.
fn checkpoints(options: &Options, agent: &Agent) -> Result<Arc<Mutex<Checkpoints>>, String> {
    let root = options.checkpoints.as_ref().ok_or("this session has none")?;
    let checkpoints = Checkpoints::new(root, agent.workspace(), agent.session_id())?;
    Ok(Arc::new(Mutex::new(checkpoints)))
}

/// A diff's lines, coloured as git colours them, the first `most` of them.
fn diff_lines(diff: &str, most: usize) -> Vec<Line> {
    let mut lines: Vec<Line> = diff
        .lines()
        .take(most)
        .map(|line| {
            let style = if line.starts_with("diff --git")
                || line.starts_with("+++ ")
                || line.starts_with("--- ")
            {
                theme::BOLD
            } else if line.starts_with('+') {
                theme::ADDED
            } else if line.starts_with('-') {
                theme::REMOVED
            } else if line.starts_with("@@") {
                theme::CODE
            } else {
                Style::PLAIN
            };
            Line::styled(style::expand_tabs(line), style)
        })
        .collect();
    let rest = diff.lines().count().saturating_sub(most);
    if rest > 0 {
        lines.push(Line::styled(format!("… {} more lines", thousands(rest as u64)), theme::DIM));
    }
    lines
}

/// The first background subagent to end: where it is in `tasks`, and its report.
async fn next_report(tasks: &mut [Task]) -> (usize, Report) {
    std::future::poll_fn(|cx| {
        for (i, task) in tasks.iter_mut().enumerate() {
            if let Poll::Ready(report) = task.run.as_mut().poll(cx) {
                return Poll::Ready((i, report));
            }
        }
        Poll::Pending
    })
    .await
}

/// `/name`, with or without more after it: a command, unless it looks like a path.
fn command(text: &str) -> Option<&str> {
    let word = text.split_whitespace().next()?;
    let name = word.strip_prefix('/')?;
    let plain = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    plain.then_some(word)
}

/// What the agent is doing, how long the turn has taken, and how to stop it.
fn busy_line(turn: &Turn) -> Line {
    let elapsed = turn.started.elapsed();
    let frame = SPINNER[(elapsed.as_millis() / SPINNER_STEP.as_millis()) as usize % SPINNER.len()];
    let doing = match (&turn.approval, &turn.phase) {
        _ if turn.interrupted => "interrupting".to_owned(),
        _ if turn.compacting => "compacting".to_owned(),
        (Some(_), _) => "waiting for your answer".to_owned(),
        (None, Phase::Waiting) => "waiting for the model".to_owned(),
        (None, Phase::Thinking) => "thinking".to_owned(),
        (None, Phase::Writing) => "writing".to_owned(),
        (None, Phase::Running) => match turn.calls.as_slice() {
            [] => "working".to_owned(),
            [(_, call)] => clip(call, 60),
            calls => format!("running {} calls", calls.len()),
        },
    };
    let mut line = Line::styled(format!("{frame} "), theme::SPINNER).with(&doing, Style::PLAIN);
    line.push(&format!(" · {}", duration(elapsed.as_secs_f64())), theme::DIM);
    if !turn.interrupted {
        line.push(" · esc to interrupt", theme::DIM);
    }
    line
}

/// The question for a command that needs approval.
fn approval_lines(approval: &Approval, width: usize) -> Vec<Line> {
    let why = approval.reasons.join("; ");
    let question = format!("Run this command? It needs approval: {why}.");
    let mut lines = vec![Line::new(), Line::styled(question, theme::NOTICE.bold())];
    let command: Vec<&str> = approval.command.lines().collect();
    let shown = command[..command.len().min(8)].join("\n");
    lines.extend(hanging(Line::styled("  $ ", theme::DIM), &shown, Style::PLAIN, width));
    if command.len() > 8 {
        lines.push(Line::styled(format!("    … {} more lines", command.len() - 8), theme::DIM));
    }
    lines.push(
        Line::styled("y", theme::BOLD)
            .with(" yes · ", theme::DIM)
            .with("a", theme::BOLD)
            .with(" yes, and this command again this session · ", theme::DIM)
            .with("n", theme::BOLD)
            .with(" no", theme::DIM),
    );
    lines
}

/// A tool call in one line, or more if it wraps, under its subagent's label if it has one.
fn call_lines(label: Option<&str>, description: &str, width: usize) -> Vec<Line> {
    let mut head = Line::new();
    if let Some(label) = label {
        head.push(&format!("  [{label}] "), theme::DIM);
    }
    head.push("● ", theme::TOOL);
    hanging(head, description, Style::PLAIN, width)
}

/// A tool's result, summarised in a line: dimmed, or red for an error.
fn result_lines(label: Option<&str>, output: &str, is_error: bool, width: usize) -> Vec<Line> {
    let mut head = Line::new();
    if let Some(label) = label {
        head.push(&format!("  [{label}] "), theme::DIM);
    }
    head.push("  ⎿ ", theme::DIM);
    let style = if is_error { theme::ERROR } else { theme::DIM };
    hanging(head, &summarise(output), style, width)
}

/// `head`, then `text` in `style`, wrapped to `width` with every row after the first indented to
/// line up under the text.
fn hanging(head: Line, text: &str, style: Style, width: usize) -> Vec<Line> {
    let indent = head.width();
    let room = width.saturating_sub(indent).max(1);
    let mut lines = Vec::new();
    for line in style::lines(text, style) {
        for row in wrap(&line, room) {
            let start =
                if lines.is_empty() { head.clone() } else { Line::plain(" ".repeat(indent)) };
            lines.push(start.with_line(row));
        }
    }
    lines
}

/// An error and its causes in one line, leaving out a cause that the message before it already
/// quotes.
fn causes(error: &anyhow::Error) -> String {
    let mut text = String::new();
    for cause in error.chain().map(ToString::to_string) {
        if !text.contains(&cause) {
            if !text.is_empty() {
                text.push_str(": ");
            }
            text.push_str(&cause);
        }
    }
    text
}

/// `path` with the home directory as `~`.
fn home_relative(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.as_os_str().len() > 1);
    match home.and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The terminal as the TUI needs it: raw mode, bracketed paste, keys reported unambiguously where
/// the terminal can (so that Shift+Enter is not Enter), and the cursor hidden, since the input box
/// draws its own. Dropping it, or a panic, puts everything back.
struct Terminal;

static ACTIVE: AtomicBool = AtomicBool::new(false);

impl Terminal {
    fn enter() -> io::Result<Self> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let terminal = Terminal;
        execute!(
            io::stdout(),
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            Hide
        )?;
        Ok(terminal)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags, DisableBracketedPaste, Show);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// An app on an 80 by 24 screen that writes to memory, with an agent whose turn is begun by
    /// `begin`; the agent makes no requests. The directory is removed on drop.
    struct Test {
        app: App<Vec<u8>>,
        dir: PathBuf,
        /// The agent's, for background subagents the tests make up.
        interrupter: Interrupter,
    }

    impl Test {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("eidra-tui-{}-{n}", std::process::id()));
            let options = Options {
                profile: Profile::builtin().unwrap(),
                workspace: dir.join("work"),
                session_root: dir.join("sessions"),
                confirm: Vec::new(),
                checkpoints: Some(dir.join("checkpoints")),
                resume: None,
            };
            std::fs::create_dir_all(&options.workspace).unwrap();
            let agent = start(&options).unwrap();
            let interrupter = agent.interrupter();
            let mut app = App::new(options, agent, Screen::new(Vec::new(), 80, 24));
            app.finished.clear();
            app.last_block = None;
            Test { app, dir, interrupter }
        }

        /// A background subagent that never ends by itself; `task_ended` ends it.
        fn add_task(&mut self, id: &str, label: &str) {
            let run: Pin<Box<dyn Future<Output = Report>>> = Box::pin(std::future::pending());
            let (id, label, interrupter) = (id.into(), label.into(), self.interrupter.clone());
            let (started, doing, session) = (Instant::now(), String::new(), self.app.session);
            self.app.tasks.push(Task { run, id, label, interrupter, started, doing, session });
        }

        fn submit(&mut self, text: &str) {
            self.app.editor.insert(text);
            self.key(KeyCode::Enter);
        }

        /// The last message the agent was given, from its session log.
        fn last_message(&self) -> String {
            let log = std::fs::read_to_string(&self.app.log).unwrap();
            let records =
                log.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap());
            let mut messages =
                records.filter(|r| r["type"] == "message" && r["message"]["role"] == "user");
            let message = messages.next_back().unwrap();
            message["message"]["content"].as_str().unwrap().to_owned()
        }

        /// Starts a turn that never ends by itself, and returns the agent it would hand back.
        fn begin(&mut self) -> Agent {
            let agent = self.app.agent.take().unwrap();
            let run: Run = Box::pin(std::future::pending());
            self.app.turn = Some(Turn::new(run, agent.interrupter()));
            agent
        }

        fn events(&mut self, events: Vec<Event>) {
            for event in events {
                self.app.event(event);
            }
        }

        /// The scrollback lines committed so far, as text.
        fn finished(&mut self) -> Vec<String> {
            self.app.finished.drain(..).map(|line| line.text()).collect()
        }

        fn live(&mut self) -> Vec<String> {
            self.app.live().iter().map(Line::text).collect()
        }

        fn key(&mut self, code: KeyCode) {
            self.app.key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }

    impl Drop for Test {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn reply(reasoning_tokens: u64) -> Event {
        let stats = RequestStats { reasoning_tokens: Some(reasoning_tokens), ..Default::default() };
        Event::Reply { stats }
    }

    fn call(id: &str, name: &str, arguments: &str) -> Event {
        Event::ToolCall { id: id.into(), name: name.into(), arguments: arguments.into() }
    }

    fn result(id: &str, name: &str, output: &str) -> Event {
        let (id, name, output) = (id.into(), name.into(), output.into());
        Event::ToolResult { id, name, output, is_error: false }
    }

    #[test]
    fn a_step_is_its_reasoning_calls_and_results_together() {
        let mut t = Test::new();
        t.begin();
        t.events(vec![
            Event::Request { number: 1 },
            Event::Reasoning { text: "Let me look.".into() },
            reply(4),
            call("1", "bash", r#"{"command": "ls"}"#),
            result("1", "bash", "a\nb\nc"),
            Event::Request { number: 2 },
            Event::Reasoning { text: "Three.".into() },
            Event::Content { text: "There are **three** files.".into() },
            reply(2),
        ]);
        assert_eq!(
            t.finished(),
            [
                "∴ thought for 4 tokens",
                "● $ ls",
                "  ⎿ a  (+2 lines)",
                "",
                "∴ thought for 2 tokens",
                "There are three files."
            ]
        );
    }

    #[test]
    fn todo_lists_and_subagent_steps() {
        let mut t = Test::new();
        t.begin();
        t.events(vec![
            call("1", "todo", r#"{"list": "- [x] read\n- [ ] write"}"#),
            Event::Todo { list: "- [x] read\n- [ ] write".into() },
            result("1", "todo", "The todo list is updated."),
            Event::Subagent {
                task: "2".into(),
                label: "find the parser".into(),
                event: Box::new(call("3", "read", r#"{"path": "src/parse.rs"}"#)),
            },
            Event::Subagent {
                task: "2".into(),
                label: "find the parser".into(),
                event: Box::new(Event::Reasoning { text: "not shown".into() }),
            },
        ]);
        assert_eq!(
            t.finished(),
            ["● todo", "    ☒ read", "    ☐ write", "  [find the parser] ● read src/parse.rs"]
        );
    }

    #[test]
    fn the_live_region_follows_the_turn() {
        let mut t = Test::new();
        assert_eq!(t.live()[1], "› Message eidra · /help for keys and commands");
        t.begin();
        t.events(vec![Event::Reasoning { text: "First thought.\nSecond thought.".into() }]);
        let live = t.live();
        assert_eq!(live[..2], ["First thought.", "Second thought."]);
        assert!(live[3].contains("thinking · 0 s · esc to interrupt"), "{live:?}");
        assert!(live[4].starts_with("────"), "{live:?}");
        assert_eq!(live[5], "› Queue message");
        assert_eq!(live.last().unwrap(), "qwen3.8-flash-next · xhigh · 0/256K");

        // The reply's text replaces its reasoning in the tail.
        t.events(vec![Event::Content { text: "# Done\n\nAll good.".into() }]);
        assert_eq!(t.live()[..3], ["Done", "", "All good."]);
    }

    #[test]
    fn approvals_wait_a_moment_before_taking_keys() {
        let mut t = Test::new();
        t.begin();
        let (answer, mut answered) = oneshot::channel();
        let approval = Approval { command: "git push".into(), reasons: vec!["it matches".into()] };
        t.app.message(Message::Approve(approval, answer));
        assert!(t.live().iter().any(|row| row.contains("Run this command?")));
        assert!(t.live().iter().any(|row| row == "  $ git push"));

        // Typed for the input box just as the question came up: ignored.
        t.key(KeyCode::Char('a'));
        assert!(answered.try_recv().is_err());
        assert!(t.app.approving());
        assert!(t.app.editor.is_empty());

        t.app.turn.as_mut().unwrap().approval.as_mut().unwrap().shown -= APPROVAL_GRACE;
        t.key(KeyCode::Char('n'));
        assert_eq!(answered.try_recv().unwrap(), Decision::Deny { note: None });
        assert!(!t.app.approving());
    }

    #[test]
    fn queued_messages_are_sent_after_a_reply_and_returned_after_an_interrupt() {
        let mut t = Test::new();
        let agent = t.begin();
        t.app.editor.insert("next one");
        t.key(KeyCode::Enter);
        assert_eq!(t.app.queued, ["next one"]);
        assert!(t.live().iter().any(|row| row == "queued › next one"));

        // Interrupted: the queued message goes back to the input box.
        t.key(KeyCode::Esc);
        assert!(t.app.turn.as_ref().unwrap().interrupted);
        t.app.turn_ended(agent, Ok(Outcome::Interrupted));
        assert!(t.app.turn.is_none());
        assert_eq!(t.app.editor.text(), "next one");
        assert!(t.finished().contains(&"! interrupted".to_owned()));

        // After a reply, it is sent at once, starting the next turn.
        let agent = t.begin();
        t.key(KeyCode::Enter);
        assert_eq!(t.app.queued, ["next one"]);
        t.app.turn_ended(agent, Ok(Outcome::Replied));
        assert!(t.app.queued.is_empty());
        assert!(t.app.turn.is_some());
        assert_eq!(t.finished(), ["", "› next one"]);
    }

    #[test]
    fn ctrl_enter_interrupts_and_sends_at_once_with_what_was_queued() {
        let mut t = Test::new();
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        let agent = t.begin();
        // With nothing typed or queued there is nothing to send, so the turn goes on.
        t.app.key(ctrl_enter);
        assert!(!t.app.turn.as_ref().unwrap().interrupted);

        t.submit("first");
        t.app.editor.insert("then this");
        t.app.key(ctrl_enter);
        assert!(t.app.turn.as_ref().unwrap().interrupted);
        assert_eq!(t.app.queued, ["first", "then this"]);

        // Once the turn has stopped, both go, rather than back to the input box.
        t.app.turn_ended(agent, Ok(Outcome::Interrupted));
        assert!(t.app.turn.is_some() && t.app.queued.is_empty() && t.app.editor.is_empty());
        assert_eq!(t.last_message(), "first\n\nthen this");
        assert!(t.finished().contains(&"! interrupted".to_owned()));
    }

    #[test]
    fn compact_runs_as_a_turn_of_its_own() {
        let mut t = Test::new();
        t.submit("/compact the API notes");
        assert!(t.app.turn.as_ref().is_some_and(|turn| turn.compacting));
        assert!(t.live().iter().any(|row| row.contains("compacting · 0 s · esc to interrupt")));
        t.submit("/compact");
        let refused =
            "! the agent is working: /compact once its turn ends, or press esc to stop it";
        assert!(t.finished().contains(&refused.to_owned()));

        // Once it has compacted, the numbers of the conversation it replaced go.
        let mut t = Test::new();
        let agent = t.begin();
        t.app.turn.as_mut().unwrap().compacting = true;
        t.app.stats = Some(RequestStats { prompt_tokens: Some(150_000), ..Default::default() });
        t.app.turn_ended(agent, Ok(Outcome::Replied));
        assert!(t.app.stats.is_none());
        assert_eq!(t.app.status_line(80).text(), "qwen3.8-flash-next · xhigh · 0/256K");
    }

    #[test]
    fn the_header_names_the_agents_md_files_read() {
        let mut t = Test::new();
        std::fs::write(t.dir.join("work/AGENTS.md"), "Use tabs.\n").unwrap();
        // A session begun now reads it. The user's own, in ~/.eidra, may come first.
        t.app.agent = Some(start(&t.app.options).unwrap());
        let line = t.app.instructions_line();
        assert!(line == "instructions: AGENTS.md" || line.ends_with(", AGENTS.md"), "{line}");
        t.app.header();
        assert_eq!(t.finished().last(), Some(&line));
    }

    #[test]
    fn ctrl_enter_sends_like_enter_when_the_agent_is_idle() {
        let mut t = Test::new();
        t.app.editor.insert("hello");
        t.app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        assert!(t.app.turn.is_some());
        assert_eq!(t.last_message(), "hello");
    }

    fn report(label: &str, text: &str) -> Report {
        let (label, text) = (label.to_owned(), text.to_owned());
        Report { task: "call_1".into(), label, text, is_error: false }
    }

    /// What brings a report to the agent, from the profile.
    fn report_message(label: &str, text: &str) -> String {
        let template = Profile::builtin().unwrap().prompts.task_report;
        template.replace("{task}", label).replace("{report}", text)
    }

    #[test]
    fn a_report_goes_to_an_idle_agent_at_once() {
        let mut t = Test::new();
        t.add_task("call_1", "build it");
        assert!(t.app.status_line(80).text().contains("· 1 task running ·"));
        t.app.event(Event::Subagent {
            task: "call_1".into(),
            label: "build it".into(),
            event: Box::new(call("2", "bash", r#"{"command": "make"}"#)),
        });
        assert_eq!(t.app.tasks[0].doing, "$ make");
        t.app.task_ended(0, report("build it", "Built calc.py."));
        assert_eq!(
            t.finished(),
            [
                "  [build it] ● $ make",
                "◆ background task \"build it\" ended after 0 s",
                "  ⎿ Built calc.py."
            ]
        );
        assert!(t.app.turn.is_some(), "the report starts a turn");
        assert_eq!(t.last_message(), report_message("build it", "Built calc.py."));
    }

    #[test]
    fn a_report_waits_for_the_turn_and_goes_before_what_was_queued() {
        let mut t = Test::new();
        let agent = t.begin();
        t.add_task("call_1", "build it");
        t.app.task_ended(0, report("build it", "Built calc.py."));
        assert_eq!(t.app.reports.len(), 1);
        assert!(t.app.status_line(80).text().contains("· 1 report waiting ·"));
        t.submit("and then?");
        t.app.turn_ended(agent, Ok(Outcome::Replied));
        assert!(t.app.turn.is_some() && t.app.reports.is_empty());
        let expected = format!("{}\n\nand then?", report_message("build it", "Built calc.py."));
        assert_eq!(t.last_message(), expected);
    }

    #[test]
    fn after_an_interrupt_a_report_waits_for_the_next_message() {
        let mut t = Test::new();
        let agent = t.begin();
        t.add_task("call_1", "build it");
        t.app.task_ended(0, report("build it", "Built calc.py."));
        t.app.turn_ended(agent, Ok(Outcome::Interrupted));
        assert!(t.app.turn.is_none() && t.app.reports.len() == 1);
        t.submit("next");
        let expected = format!("{}\n\nnext", report_message("build it", "Built calc.py."));
        assert_eq!(t.last_message(), expected);
    }

    #[test]
    fn a_task_of_an_earlier_session_reports_to_no_one() {
        let mut t = Test::new();
        t.add_task("call_1", "build it");
        t.submit("/new");
        t.app.task_ended(0, report("build it", "Built calc.py."));
        assert!(t.app.turn.is_none() && t.app.reports.is_empty());
    }

    #[test]
    fn background_tasks_are_listed_and_stopped() {
        let mut t = Test::new();
        t.add_task("call_1", "one");
        t.add_task("call_2", "two");
        t.submit("/tasks");
        assert_eq!(t.finished(), ["  1  one · 0 s · starting", "  2  two · 0 s · starting"]);
        t.submit("/stop 2");
        assert_eq!(t.finished(), ["", "stopping \"two\"; the agent gets its report"]);
        t.submit("/stop 5");
        assert_eq!(t.finished(), ["", "! there is no background task 5; /tasks lists them"]);
    }

    #[test]
    fn quitting_waits_for_background_tasks_to_stop() {
        let mut t = Test::new();
        t.add_task("call_1", "build it");
        t.app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(t.app.quitting && !t.app.quit);
        t.app.task_ended(0, report("build it", "It was interrupted."));
        assert!(t.app.quit && t.app.turn.is_none());
    }

    #[test]
    fn ctrl_c_interrupts_then_clears_then_quits() {
        let mut t = Test::new();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let agent = t.begin();
        t.app.key(ctrl_c);
        assert!(t.app.turn.as_ref().unwrap().interrupted);
        t.app.turn_ended(agent, Ok(Outcome::Interrupted));
        t.app.editor.insert("draft");
        t.app.key(ctrl_c);
        assert!(t.app.editor.is_empty() && !t.app.quit);
        t.app.key(ctrl_c);
        assert!(t.app.quit);
    }

    #[tokio::test]
    async fn a_resumed_session_shows_its_conversation() {
        let mut t = Test::new();
        // A log as a session leaves it: a reply, a call and its result, and a reply to that.
        let lines = [
            r#"{"type":"message","message":{"role":"user","content":"what is here?"}}"#,
            r#"{"type":"message","message":{"role":"assistant","reasoning_content":"Look.","tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]},"stats":{"reasoning_tokens":3}}"#,
            r#"{"type":"message","message":{"role":"tool","tool_call_id":"c1","content":"a.txt"}}"#,
            r#"{"type":"message","message":{"role":"assistant","content":"Just **a.txt**."},"stats":{"prompt_tokens":2000,"completion_tokens":20}}"#,
        ];
        let mut log = std::fs::read_to_string(&t.app.log).unwrap();
        for line in lines {
            log.push_str(line);
            log.push('\n');
        }
        std::fs::write(&t.app.log, log).unwrap();
        let options = Options { resume: Some(t.app.log.clone()), ..t.app.options.clone() };
        let agent = Agent::resume(options.profile.clone(), agent_options(&options), &t.app.log);
        t.app = App::new(options, agent.unwrap(), Screen::new(Vec::new(), 80, 24));
        t.app.finished.clear();
        t.app.last_block = None;
        t.app.resumed().await;
        assert_eq!(
            t.finished(),
            [
                "› what is here?",
                "",
                "∴ thought for 3 tokens",
                "● $ ls",
                "  ⎿ a.txt",
                "",
                "Just a.txt.",
                "",
                "resumed: the conversation goes on from here (5 messages)",
            ]
        );
        assert!(t.live().last().unwrap().contains("2.0K/256K"));
    }

    #[tokio::test]
    async fn a_command_after_a_bang_runs_and_the_agent_hears_of_it() {
        let mut t = Test::new();
        t.submit("!echo hi; echo oops >&2; exit 3");
        assert!(t.live().iter().any(|row| row.contains("running echo hi")));
        // A message sent meanwhile waits for the command.
        t.submit("why did it fail?");
        finish_job(&mut t).await;
        assert_eq!(
            t.finished(),
            [
                "! echo hi; echo oops >&2; exit 3",
                "",
                "  hi",
                "  oops",
                "  ⎿ failed (exit code 3)",
                "",
                "› why did it fail?",
            ]
        );
        let message = t.last_message();
        assert_eq!(
            message,
            "[The user ran this command in the workspace: `echo hi; echo oops >&2; exit 3`. It \
             failed (exit code 3), and printed:]\n\nhi\noops\n\nwhy did it fail?"
        );
    }

    #[tokio::test]
    async fn stopping_a_command_starts_nothing_and_a_turn_waits_for_one() {
        let mut t = Test::new();
        t.submit("!sleep 30");
        t.submit("then explain");
        t.key(KeyCode::Esc);
        // Nothing starts by itself after the user stopped things: the message goes back.
        assert!(t.app.turn.is_none() && t.app.job.is_none());
        assert_eq!(t.app.editor.text(), "then explain");
        assert!(t.app.notes.is_empty());

        // A turn that ends while a command runs leaves what was queued for the command's end.
        t.app.editor.clear();
        let agent = t.begin();
        t.submit("!echo out");
        t.submit("now fix it");
        t.app.turn_ended(agent, Ok(Outcome::Replied));
        assert_eq!(t.app.queued, ["now fix it"]);
        finish_job(&mut t).await;
        assert!(t.last_message().starts_with("[The user ran this command in the workspace: `echo"));
        assert!(t.last_message().ends_with("now fix it"));
    }

    #[tokio::test]
    async fn esc_stops_a_command() {
        let mut t = Test::new();
        t.submit("!sleep 30");
        let started = std::time::Instant::now();
        t.key(KeyCode::Esc);
        assert!(t.app.job.is_none() && t.app.command.is_none());
        assert!(t.finished().contains(&"! stopped `sleep 30`".to_owned()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_mentioned_file_goes_with_the_message_and_tab_completes_it() {
        let mut t = Test::new();
        let work = t.dir.join("work");
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(work.join("src/calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
        t.app.editor.insert("what does @sr");
        t.key(KeyCode::Tab);
        assert_eq!(t.app.editor.text(), "what does @src/");
        t.key(KeyCode::Tab);
        assert_eq!(t.app.editor.text(), "what does @src/calc.py ");
        t.app.editor.insert("do?");
        t.key(KeyCode::Enter);
        assert_eq!(
            t.finished(),
            ["› what does @src/calc.py do?", "  ⎿ attached src/calc.py: 2 lines"]
        );
        assert_eq!(
            t.last_message(),
            "what does @src/calc.py do?\n\n[The user attached src/calc.py, shown as read shows \
             it:]\n     1\tdef add(a, b):\n     2\t    return a + b"
        );
        // Tab elsewhere is still four spaces.
        t.app.agent.get_or_insert_with(|| start(&t.app.options).unwrap());
        t.app.turn = None;
        t.key(KeyCode::Tab);
        assert_eq!(t.app.editor.text(), "    ");
    }

    #[test]
    fn the_end_of_a_quiet_turn_is_drawn() {
        let mut t = Test::new();
        let agent = t.begin();
        t.app.draw().unwrap();
        assert!(!t.app.dirty);
        t.app.turn_ended(agent, Ok(Outcome::Replied));
        assert!(t.app.dirty, "the busy line would stay on the screen");
        assert!(!t.live().iter().any(|row| row.contains("esc to interrupt")));
    }

    /// Runs the /diff or /undo the app started, to its end.
    async fn finish_job(test: &mut Test) {
        let done = job_done(&mut test.app.job).await;
        test.app.job_ended(done);
    }

    #[tokio::test]
    async fn undo_puts_files_back_and_tells_the_agent() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return eprintln!("no git: skipped");
        }
        let mut t = Test::new();
        let work = t.dir.join("work");
        std::fs::write(work.join("a.py"), "a = 1\n").unwrap();
        let checkpoints = Arc::clone(t.app.checkpoints.as_ref().unwrap());
        // A turn, as send() runs one: a snapshot before and after.
        checkpoints.lock().await.begin("make a two\nplease").await.unwrap();
        std::fs::write(work.join("a.py"), "a = 2\n").unwrap();
        std::fs::write(work.join("b.py"), "b = 1\n").unwrap();
        checkpoints.lock().await.end().await.unwrap();

        t.submit("/diff");
        finish_job(&mut t).await;
        let shown = t.finished();
        assert_eq!(shown[0], "changes since the last turn (turn 1) began");
        assert!(shown.contains(&"-a = 1".to_owned()) && shown.contains(&"+b = 1".to_owned()));

        t.submit("/undo");
        // A message sent meanwhile waits for it.
        t.submit("now make it three");
        assert_eq!(t.app.queued, ["now make it three"]);
        finish_job(&mut t).await;
        assert_eq!(std::fs::read_to_string(work.join("a.py")).unwrap(), "a = 1\n");
        assert!(!work.join("b.py").exists());
        let shown = t.finished();
        assert_eq!(shown[1], "↶ undid turn 1 · make a two");
        assert_eq!(shown[2], "  ⎿ put back: a.py");
        assert_eq!(shown[3], "  ⎿ removed, since the turn made them: b.py");
        // The queued message went, after the note.
        let message = t.last_message();
        assert!(
            message.starts_with("[The user undid what your turn that began with \"make a two\"")
                && message.contains("put back as they were before it: a.py.")
                && message.ends_with("]\n\nnow make it three"),
            "{message}"
        );
        assert!(t.app.notes.is_empty());

        t.app.turn = None;
        t.app.agent.get_or_insert_with(|| start(&t.app.options).unwrap());
        t.submit("/undo");
        finish_job(&mut t).await;
        assert_eq!(
            t.finished().last().unwrap(),
            "no turn of this session is left that changed files"
        );
    }

    #[test]
    fn commands_are_words_not_paths() {
        assert_eq!(command("/help"), Some("/help"));
        assert_eq!(command("/new please"), Some("/new"));
        assert_eq!(command("/etc/hosts is wrong"), None);
        assert_eq!(command("fix /tmp"), None);
        assert_eq!(command("/"), None);
    }

    #[test]
    fn error_causes_are_not_repeated() {
        let root = anyhow::anyhow!("connection refused");
        let error = root.context("sending to http://x: connection refused").context("request 1");
        assert_eq!(causes(&error), "request 1: sending to http://x: connection refused");
    }

    #[test]
    fn hanging_text_lines_up_under_its_start() {
        let lines = hanging(Line::plain("› "), "one two three\nfour", Style::PLAIN, 9);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert_eq!(texts, ["› one two", "  three", "  four"]);
    }
}
