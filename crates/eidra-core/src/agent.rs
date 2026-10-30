//! The agent loop (docs/DESIGN.md §4): build a request from the session log, stream the reply,
//! run its tool calls, append everything to the log, repeat. The log only grows, and the system
//! prompt, tools and effort are fixed when the session starts, so every request begins with the
//! one before it (§3).

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use eidra_llm::{
    AssistantTurn, ChatRequest, Client, Content, ImageUrl, Message, Part, StreamEvent, ToolCall,
    Usage,
};
use futures_util::future::join_all;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::confirm::ConfirmPolicy;
use crate::display::thousands;
use crate::event::{Approval, Decision, Event, Frontend, RequestStats};
use crate::home::eidra_home;
use crate::media;
use crate::profile::Profile;
use crate::session::{Header, Session, text_of};
use crate::skills::{self, Skill, Skills};
use crate::text::{fnv1a, truncate_middle, utc_datetime};
use crate::tools::{self, ToolOutput, Toolbox};

pub struct AgentOptions {
    /// The directory the agent works in.
    pub workspace: PathBuf,
    /// Where session logs go.
    pub session_root: PathBuf,
    /// Unattended: every reply must call a tool, the session ends with `finish`, and commands that
    /// need approval are declined.
    pub headless: bool,
    /// Confirm patterns on top of the profile's.
    pub confirm: Vec<String>,
    /// A Unix socket that reaches the model server, for an agent with no network of its own.
    pub model_socket: Option<PathBuf>,
    /// Where skills are (docs/DESIGN.md §11.1): `~/.agents/skills`, or none.
    pub skills: Option<PathBuf>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The model called finish, with this summary.
    Finished(String),
    /// The model answered without calling a tool; the turn is the user's.
    Replied,
    /// The request limit was reached.
    RequestLimit,
    /// The prompt grew past the profile's context limit, to this many tokens.
    ContextLimit(u64),
    /// The user stopped the run (see [`Interrupter`]). The session can go on.
    Interrupted,
}

/// Stops an agent's run from outside, as Esc does in the TUI. A request in flight is dropped, a
/// running tool is stopped, and the log stays a valid conversation: the reply's unanswered calls
/// get results that say they were interrupted.
#[derive(Clone)]
pub struct Interrupter(Arc<watch::Sender<bool>>);

impl Interrupter {
    pub fn interrupt(&self) {
        self.0.send_replace(true);
    }
}

pub struct Agent {
    client: Client,
    profile: Profile,
    workspace: PathBuf,
    session: Session,
    tools: Toolbox,
    confirm: ConfirmPolicy,
    headless: bool,
    requests: usize,
    /// What a subagent inherits: where logs go, the confirm patterns beyond the profile's, the way
    /// to the model, and the date in the system prompt (so that its prompt starts as this one's
    /// does).
    session_root: PathBuf,
    extra_confirm: Vec<String>,
    model_socket: Option<PathBuf>,
    date: String,
    /// Where skills are, for the user to invoke and for a compaction to load again.
    skills: Option<PathBuf>,
    interrupt: Arc<watch::Sender<bool>>,
    /// A run was stopped before the model replied. The next message says so: otherwise the
    /// model takes up the request it was stopped on again.
    unanswered: bool,
    /// Subagents this agent started in the background that have not ended yet.
    background: Arc<AtomicUsize>,
    /// Tokens the conversation took at the last reply: its prompt and the reply.
    used: u64,
    /// A summary came back empty, so this conversation runs on to the limit instead of asking for
    /// one after every reply.
    uncompactable: bool,
    /// When an unattended run's time is up, if it has a limit (see [`Agent::set_time_limit`]).
    deadline: Option<(Instant, Duration)>,
    /// The model has been told its budget is nearly spent.
    warned: bool,
}

/// A subagent started by a `task` call with `background`. The call returns at once, and the
/// frontend runs the subagent while the conversation goes on; when it ends, its report goes back
/// to the agent that started it as a message of its own (see [`Agent::report_message`]).
pub struct Background {
    /// The id of the `task` call that started it.
    pub task: String,
    /// The task's description.
    pub label: String,
    agent: Box<Agent>,
    max_requests: usize,
    /// Its place among the starting agent's background subagents, given back when it ends.
    _running: Running,
}

/// What a background subagent reported when it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The id of the `task` call that started it.
    pub task: String,
    pub label: String,
    pub text: String,
    pub is_error: bool,
}

/// Counts a background subagent as running until it is dropped.
struct Running(Arc<AtomicUsize>);

impl Running {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Running(Arc::clone(count))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whom an agent works for.
enum Role {
    /// The user, in the loop or not.
    Main,
    /// Another agent: the session that started it, and the task's description.
    Subagent { parent: String, task: String },
}

impl Agent {
    pub fn start(profile: Profile, options: AgentOptions) -> Result<Self> {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let (year, month, day, ..) = utc_datetime(secs);
        Self::create(profile, options, Role::Main, format!("{year:04}-{month:02}-{day:02}"))
    }

    fn create(profile: Profile, options: AgentOptions, role: Role, date: String) -> Result<Self> {
        let workspace = fs::canonicalize(&options.workspace)
            .with_context(|| format!("the working directory {}", options.workspace.display()))?;
        let api_key = profile
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|key| !key.is_empty());
        let socket = options.model_socket.as_deref();
        let client = Client::new(&profile.base_url, api_key, socket, profile.tls_pin.as_deref())?;

        let subagent = matches!(role, Role::Subagent { .. });
        let mut definitions = tools::definitions(&profile, options.headless, subagent)?;
        // The skills the model may load, if the session has `skill`. With none, the tool goes,
        // and the system prompt says nothing of skills (§11.1).
        let found = match &options.skills {
            Some(dir) if definitions.iter().any(|tool| tool.function.name == "skill") => {
                skills::find(dir)
            }
            _ => Skills::default(),
        };
        let listing = found.listing();
        if listing.is_empty() {
            definitions.retain(|tool| tool.function.name != "skill");
        }
        let (parent, task) = match role {
            Role::Main => (None, None),
            Role::Subagent { parent, task } => (Some(parent), Some(task)),
        };
        let header = Header {
            version: 1,
            id: String::new(),
            created: String::new(),
            workspace: workspace.display().to_string(),
            headless: options.headless,
            profile: profile.name.clone(),
            profile_hash: profile.hash.clone(),
            base_url: profile.base_url.clone(),
            model: profile.model.clone(),
            reasoning_effort: profile.request.reasoning_effort.clone(),
            tools: definitions.clone(),
            parent,
            task,
        };
        let mut session = Session::create(&options.session_root, header)?;
        let tools =
            Toolbox::new(&profile, &workspace, session.dir(), definitions, options.skills.clone())?;

        let mode = if subagent {
            &profile.prompts.subagent
        } else if options.headless {
            &profile.prompts.headless
        } else {
            &profile.prompts.interactive
        };
        // Reaching the model through a socket means there is no network otherwise (§13).
        let network =
            if options.model_socket.is_some() { &profile.prompts.no_network[..] } else { "" };
        let listed = match listing.is_empty() {
            true => String::new(),
            false => profile.prompts.skills.replace("{skills}", &listing),
        };
        let system = profile.system_prompt(&[
            ("workspace", &workspace.display().to_string()),
            ("date", &date),
            ("mode", &format!("{mode}{network}")),
            ("agents", &agents_md(&workspace)),
            // Last, so that nothing in a skill's description is taken for another variable.
            ("skills", &listed),
        ]);
        session.push(Message::system(system), None)?;

        let mut patterns = profile.confirm.commands.clone();
        patterns.extend(options.confirm.iter().cloned());
        Ok(Agent {
            client,
            confirm: ConfirmPolicy::new(&patterns),
            profile,
            workspace,
            session,
            tools,
            headless: options.headless,
            requests: 0,
            session_root: options.session_root,
            extra_confirm: options.confirm,
            model_socket: options.model_socket,
            date,
            skills: options.skills,
            interrupt: Arc::new(watch::channel(false).0),
            unanswered: false,
            background: Arc::new(AtomicUsize::new(0)),
            used: 0,
            uncompactable: false,
            deadline: None,
            warned: false,
        })
    }

    /// Goes on with the session logged at `log` (docs/DESIGN.md §14), in its workspace and with
    /// its system prompt and tools, which a session keeps for good; `profile` gives the rest. The
    /// calls its last reply left without results get results that say so, and the next message
    /// says the turn was cut short, if it was.
    pub fn resume(profile: Profile, options: AgentOptions, log: &Path) -> Result<Self> {
        let (mut session, read) = Session::open(log)?;
        let header = session.header().clone();
        if header.parent.is_some() {
            bail!("{} is a subagent's session, which only its parent goes on with", header.id);
        }
        if header.headless != options.headless {
            let (ran, with) = if header.headless {
                ("unattended", "eidra exec")
            } else {
                ("interactively", "eidra without a command")
            };
            bail!("session {} ran {ran}; go on with it with {with}", header.id);
        }
        let workspace = PathBuf::from(&header.workspace);
        if !workspace.is_dir() {
            bail!("session {}'s workspace {} is not there any more", header.id, header.workspace);
        }
        let api_key = profile
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|key| !key.is_empty());
        let socket = options.model_socket.as_deref();
        let client = Client::new(&profile.base_url, api_key, socket, profile.tls_pin.as_deref())?;
        let tools = Toolbox::new(
            &profile,
            &workspace,
            session.dir(),
            header.tools.clone(),
            options.skills.clone(),
        )?;

        // The last reply, and whether the turn it belongs to ended: with a reply that calls
        // nothing, or with finish.
        let messages = session.messages().to_vec();
        let last = messages.iter().rposition(|m| matches!(m, Message::Assistant(_)));
        let mut unanswered = messages.len() > 1;
        if let Some(i) = last
            && let Message::Assistant(reply) = &messages[i]
        {
            let answered: Vec<&str> = messages[i + 1..]
                .iter()
                .filter_map(|m| match m {
                    Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                    _ => None,
                })
                .collect();
            for call in reply.tool_calls.iter().filter(|c| !answered.contains(&c.id.as_str())) {
                let text = "Error: not run, because eidra stopped before this call.";
                session.push(Message::tool(call.id.clone(), text), None)?;
            }
            let finished = reply.tool_calls.iter().any(|c| c.function.name == "finish");
            unanswered = !finished && (!reply.tool_calls.is_empty() || i + 1 < messages.len());
        }
        // Requests made before, so that seeds go on from them, and the context the last one took.
        let replies = read.messages.iter().filter(|(m, _)| matches!(m, Message::Assistant(_)));
        let requests = replies.count() + read.starts.len() - 1;
        let start = read.starts.last().copied().unwrap_or(0);
        let used = read.messages[start..]
            .iter()
            .rev()
            .find(|(m, _)| matches!(m, Message::Assistant(_)))
            .and_then(|(_, stats)| stats.as_ref())
            .map_or(0, |s| s.prompt_tokens.unwrap_or(0) + s.completion_tokens.unwrap_or(0));
        session.resumed(&profile.hash)?;

        let mut patterns = profile.confirm.commands.clone();
        patterns.extend(options.confirm.iter().cloned());
        Ok(Agent {
            client,
            confirm: ConfirmPolicy::new(&patterns),
            date: header.created.get(..10).unwrap_or_default().to_owned(),
            profile,
            workspace,
            session,
            tools,
            headless: options.headless,
            requests,
            session_root: options.session_root,
            extra_confirm: options.confirm,
            model_socket: options.model_socket,
            skills: options.skills,
            interrupt: Arc::new(watch::channel(false).0),
            unanswered,
            background: Arc::new(AtomicUsize::new(0)),
            used,
            uncompactable: false,
            deadline: None,
            warned: false,
        })
    }

    /// A text file the user's next message mentions, as text for the message: what `read` would
    /// show of it, in the profile's `attached_file` wrapping. The model has then seen it.
    pub fn attach_file(&mut self, path: &str) -> Result<String, String> {
        let listing = self.tools.read_mentioned(path)?;
        let template = &self.profile.prompts.attached_file;
        Ok(template.replace("{path}", path).replace("{content}", listing.trim_end()))
    }

    /// A skill the user invoked with `/name`, as text to follow their message: its instructions,
    /// with `$ARGUMENTS` replaced by what they typed after the name, in the profile's `skill`
    /// wrapping. Also how many lines its instructions have.
    pub fn invoke_skill(&self, name: &str, arguments: &str) -> Result<(String, usize), String> {
        let found = skills::find(self.skills.as_deref().ok_or("there are no skills")?);
        let skill = found
            .get(name)
            .filter(|skill| skill.user)
            .ok_or_else(|| format!("there is no skill called {name}"))?;
        let instructions = skill.instructions()?.replace("$ARGUMENTS", arguments.trim());
        Ok((self.wrap_skill(skill, &instructions), instructions.lines().count()))
    }

    /// A skill's instructions in the profile's `skill` wrapping, cut to `[tools] output_limit` as
    /// a tool's result is, with where to read the rest.
    fn wrap_skill(&self, skill: &Skill, instructions: &str) -> String {
        let note = format!("; the whole skill is in {}", skill.dir.join("SKILL.md").display());
        let instructions = truncate_middle(instructions, self.profile.tools.output_limit, &note);
        skill.wrapped(&self.profile.prompts.skill, &instructions)
    }

    /// The skills whose instructions the conversation holds, from `skill` or because the user
    /// invoked them, as they are on disk now.
    fn loaded_skills(&self) -> Vec<(Skill, String)> {
        let Some(dir) = &self.skills else {
            return Vec::new();
        };
        let texts: Vec<String> = self
            .session
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::User { content } | Message::Tool { content, .. } => Some(text_of(content)),
                _ => None,
            })
            .collect();
        let template = &self.profile.prompts.skill;
        skills::find(dir)
            .skills
            .into_iter()
            .filter(|skill| {
                let heading = skill.heading(template);
                texts.iter().any(|text| text.contains(&heading))
            })
            .filter_map(|skill| {
                let instructions = skill.instructions().ok()?;
                Some((skill, instructions))
            })
            .collect()
    }

    /// The time an unattended run has, from now. Near its end, and near the end of a run's
    /// requests, the model is told what is left (`[prompts] budget`); eidra does not stop at it.
    pub fn set_time_limit(&mut self, limit: Duration) {
        self.deadline = Some((Instant::now() + limit, limit));
    }

    /// What is left of the run's budget, once 85% of its requests or time are spent: the text for
    /// the profile's `budget` note. Said once, and only to an unattended agent, which has
    /// `finish`.
    fn budget_left(&mut self, requests_left: usize, max_requests: usize) -> Option<String> {
        if self.warned || !self.headless {
            return None;
        }
        let mut left = Vec::new();
        if requests_left * 100 <= max_requests * 15 {
            let plural = if requests_left == 1 { "" } else { "s" };
            left.push(format!("{requests_left} request{plural}"));
        }
        if let Some((deadline, limit)) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.as_secs_f64() <= limit.as_secs_f64() * 0.15 {
                left.push(match remaining.as_secs().div_ceil(60) {
                    0 | 1 => "a minute".to_owned(),
                    minutes => format!("{minutes} minutes"),
                });
            }
        }
        if left.is_empty() {
            return None;
        }
        self.warned = true;
        Some(self.profile.prompts.budget.replace("{left}", &left.join(" and ")))
    }

    /// Whether the profile differs from the one the session began with. The session keeps its
    /// system prompt and tools either way.
    pub fn profile_changed(&self) -> bool {
        self.session.header().profile_hash != self.profile.hash
    }

    /// A subagent for one `task` call: a new, unattended session in the same workspace, with this
    /// agent's profile and confirm patterns, and the task's prompt as its first message.
    fn subagent(&self, description: &str, prompt: &str) -> Result<Agent> {
        let options = AgentOptions {
            workspace: self.workspace.clone(),
            session_root: self.session_root.clone(),
            headless: true,
            confirm: self.extra_confirm.clone(),
            model_socket: self.model_socket.clone(),
            skills: self.skills.clone(),
        };
        let role = Role::Subagent {
            parent: self.session.header().id.clone(),
            task: description.to_owned(),
        };
        let mut agent = Agent::create(self.profile.clone(), options, role, self.date.clone())?;
        agent.push_user(prompt)?;
        Ok(agent)
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn session_id(&self) -> &str {
        &self.session.header().id
    }

    /// Tokens the conversation took at the last reply, its prompt and the reply; 0 after a
    /// compaction, until the next reply.
    pub fn context_used(&self) -> u64 {
        self.used
    }

    /// The AGENTS.md files whose instructions the system prompt holds, as the session read them
    /// when it began: the user's own in eidra's directory, then the workspace's.
    pub fn instructions(&self) -> Vec<PathBuf> {
        let Some(Message::System { content: Content::Text(system) }) =
            self.session.messages().first()
        else {
            return Vec::new();
        };
        system
            .lines()
            .filter_map(|line| line.strip_prefix(INSTRUCTIONS_FROM))
            .map(PathBuf::from)
            .collect()
    }

    /// The session's log file.
    pub fn session_log(&self) -> &Path {
        self.session.path()
    }

    pub fn interrupter(&self) -> Interrupter {
        Interrupter(Arc::clone(&self.interrupt))
    }

    /// The message that brings a background subagent's report to this agent.
    pub fn report_message(&self, report: &Report) -> String {
        let template = &self.profile.prompts.task_report;
        template.replace("{task}", &report.label).replace("{report}", &report.text)
    }

    pub fn push_user(&mut self, text: &str) -> Result<()> {
        self.push_user_with(text, Vec::new())
    }

    /// A message from the user with images attached, after its text.
    pub fn push_user_with(&mut self, text: &str, images: Vec<media::Image>) -> Result<()> {
        let text = if std::mem::take(&mut self.unanswered) {
            format!("{}\n\n{text}", self.profile.prompts.interrupted)
        } else {
            text.to_owned()
        };
        if images.is_empty() {
            return self.session.push(Message::user(text), None);
        }
        let mut parts = vec![Part::Text { text }];
        parts.extend(
            images
                .into_iter()
                .map(|image| Part::ImageUrl { image_url: ImageUrl { url: image.url } }),
        );
        self.session.push(Message::User { content: Content::Parts(parts) }, None)
    }

    /// Requests and runs tools until the model finishes or replies, or a limit is reached.
    pub async fn run<F: Frontend>(
        &mut self,
        frontend: &mut F,
        max_requests: usize,
    ) -> Result<Outcome> {
        // An interrupt from before this run is not for it.
        self.interrupt.send_replace(false);
        for step in 0..max_requests {
            if self.profile.context.compact_at.is_some_and(|at| self.used > at)
                && !self.uncompactable
                && let Some(outcome) = self.compact(frontend, None).await?
            {
                return Ok(outcome);
            }
            self.requests += 1;
            frontend.event(Event::Request { number: self.requests });
            let request = self.request()?;
            let mut stop = self.interrupt.subscribe();
            let reply = tokio::select! {
                reply = self.client.chat(&request, |event| match event {
                    StreamEvent::Reasoning(text) => frontend.event(Event::Reasoning { text }),
                    StreamEvent::Content(text) => frontend.event(Event::Content { text }),
                    StreamEvent::ToolCall { .. } | StreamEvent::ToolArguments { .. } => {}
                }) => Some(reply),
                _ = stop.wait_for(|stop| *stop) => None,
            };
            // Nothing of a reply that was cut short goes into the log.
            let Some(reply) = reply else {
                self.unanswered = true;
                return Ok(Outcome::Interrupted);
            };
            let turn = match reply {
                Ok(turn) => turn,
                Err(e) => {
                    // A prompt the server cannot fit at all: as far as the session can go.
                    if let Some(tokens) = context_exceeded(&e) {
                        let tokens = tokens.unwrap_or(self.used);
                        self.session.end("context_limit", &tokens.to_string())?;
                        return Ok(Outcome::ContextLimit(tokens));
                    }
                    return Err(e).with_context(|| format!("request {}", self.requests));
                }
            };
            let stats = request_stats(&turn, request.seed);
            frontend.event(Event::Reply { stats: stats.clone() });
            let AssistantTurn { message, finish_reason, .. } = turn;
            let calls = message.tool_calls.clone();
            self.session.push(Message::Assistant(message), Some(&stats))?;

            let used = stats.prompt_tokens.unwrap_or(0) + stats.completion_tokens.unwrap_or(0);
            self.used = used;
            if used > self.profile.context.limit {
                self.session.end("context_limit", &used.to_string())?;
                return Ok(Outcome::ContextLimit(used));
            }
            let cut_off = finish_reason.as_deref() == Some("length");
            if cut_off {
                frontend.event(Event::Notice {
                    text: "the reply reached max_tokens and was cut off".into(),
                });
            }
            if calls.is_empty() {
                let nudge = if cut_off {
                    self.cut_off_message()
                } else if self.headless {
                    self.profile.prompts.no_tool_call.clone()
                } else {
                    return Ok(Outcome::Replied);
                };
                self.session.push(Message::user(nudge), None)?;
                continue;
            }

            let mut finished = None;
            let mut next = 0;
            while next < calls.len() {
                // A reply cut off at max_tokens may end in a call that was cut off too.
                let truncated = |i: usize| cut_off && i + 1 == calls.len();
                // `task` calls next to each other run together, as subagents side by side.
                let mut end = next;
                while end < calls.len()
                    && calls[end].function.name == "task"
                    && self.has_tool("task")
                    && !truncated(end)
                {
                    end += 1;
                }
                let batch = &calls[next..end.max(next + 1)];
                for call in batch {
                    frontend.event(Event::ToolCall {
                        id: call.id.clone(),
                        name: call.function.name.clone(),
                        arguments: call.function.arguments.clone(),
                    });
                }
                let mut stop = self.interrupt.subscribe();
                let outputs = tokio::select! {
                    outputs = async {
                        if end > next {
                            self.run_subagents(batch, frontend).await
                        } else if truncated(next) {
                            vec![ToolOutput::text(
                                format!("Error: {}", self.cut_off_message()),
                                true,
                            )]
                        } else {
                            vec![self.tools.call(&batch[0], &mut self.confirm, frontend).await]
                        }
                    } => Some(outputs),
                    _ = stop.wait_for(|stop| *stop) => None,
                };
                let Some(outputs) = outputs else {
                    // Every call in the reply still needs a result, or the log is no longer a
                    // conversation the server takes.
                    for (i, call) in calls.iter().enumerate().skip(next) {
                        let text = if i < next + batch.len() {
                            "Error: the user interrupted this call before it finished."
                        } else {
                            "Error: not run, because the user interrupted the reply."
                        };
                        frontend.event(Event::ToolResult {
                            id: call.id.clone(),
                            name: call.function.name.clone(),
                            output: text.into(),
                            is_error: true,
                        });
                        self.session.push(Message::tool(call.id.clone(), text), None)?;
                    }
                    return Ok(Outcome::Interrupted);
                };
                for (call, mut output) in batch.iter().zip(outputs) {
                    frontend.event(Event::ToolResult {
                        id: call.id.clone(),
                        name: call.function.name.clone(),
                        output: output.text.clone(),
                        is_error: output.is_error,
                    });
                    // The end of the reply's last result says when the budget is nearly spent:
                    // the prompt only grows at its end.
                    let last = calls.last().is_some_and(|c| c.id == call.id);
                    if last
                        && output.finish.is_none()
                        && let Some(note) = self.budget_left(max_requests - step - 1, max_requests)
                    {
                        output.text.push_str(&note);
                    }
                    let message = tool_message(&call.id, output.text, output.images);
                    self.session.push(message, None)?;
                    if output.finish.is_some() {
                        finished = output.finish;
                    }
                }
                next += batch.len();
            }
            if let Some(summary) = finished {
                self.session.end("finished", &summary)?;
                return Ok(Outcome::Finished(summary));
            }
        }
        self.session.end("request_limit", "")?;
        Ok(Outcome::RequestLimit)
    }

    /// Summarises the conversation now, as the user asked with `/compact`, and goes on from the
    /// summary (docs/DESIGN.md §8). `focus`, unless empty, says what the summary should keep.
    /// Returns an outcome only if the user stopped it, and the conversation is as it was, or the
    /// server could not fit it.
    pub async fn compact_now<F: Frontend>(
        &mut self,
        frontend: &mut F,
        focus: &str,
    ) -> Result<Option<Outcome>> {
        // An interrupt from before this is not for it.
        self.interrupt.send_replace(false);
        if !self.session.messages().iter().any(|m| matches!(m, Message::Assistant(_))) {
            frontend.event(Event::Notice { text: "there is nothing to compact yet".into() });
            return Ok(None);
        }
        // Stopped, it leaves the conversation as it was, and the last reply was not cut short.
        let unanswered = self.unanswered;
        let outcome = self.compact(frontend, Some(focus)).await?;
        if outcome == Some(Outcome::Interrupted) {
            self.unanswered = unanswered;
        }
        Ok(outcome)
    }

    /// Summarises the conversation and goes on in a new one that starts with the same system
    /// prompt and the summary (docs/DESIGN.md §8): by itself past `[context] compact_at`, or as
    /// the user asked, with what they want kept. Returns an outcome only if the run ends here:
    /// the user stopped it, and the conversation is as it was, or the server could not fit it.
    async fn compact<F: Frontend>(
        &mut self,
        frontend: &mut F,
        asked: Option<&str>,
    ) -> Result<Option<Outcome>> {
        let tokens = self.used;
        let text = match asked {
            None => format!(
                "the conversation has reached {} tokens: summarising it, to go on from the summary",
                thousands(tokens)
            ),
            Some(_) => format!(
                "summarising the conversation of {} tokens, to go on from the summary",
                thousands(tokens)
            ),
        };
        frontend.event(Event::Notice { text });
        self.requests += 1;
        frontend.event(Event::Request { number: self.requests });
        let mut prompt = self.profile.prompts.compact.clone();
        if let Some(focus) = asked.map(str::trim).filter(|focus| !focus.is_empty()) {
            prompt.push_str(&self.profile.prompts.compact_focus.replace("{focus}", focus));
        }
        let mut messages = self.session.messages().to_vec();
        messages.push(Message::user(prompt));
        let header = self.session.header();
        let mut request =
            reply_request(&self.profile, header, messages, seed(&header.id, self.requests))?;
        // Not "none": radiance takes the tools out of the prompt for that, and the whole
        // conversation would be computed again. The prompt asks for no calls; any are ignored.
        request.tool_choice = Some("auto".into());
        let mut stop = self.interrupt.subscribe();
        let reply = tokio::select! {
            reply = self.client.chat(&request, |_| {}) => Some(reply),
            _ = stop.wait_for(|stop| *stop) => None,
        };
        let Some(reply) = reply else {
            self.unanswered = true;
            return Ok(Some(Outcome::Interrupted));
        };
        let turn = match reply {
            Ok(turn) => turn,
            Err(e) => {
                if let Some(tokens) = context_exceeded(&e) {
                    let tokens = tokens.unwrap_or(self.used);
                    self.session.end("context_limit", &tokens.to_string())?;
                    return Ok(Some(Outcome::ContextLimit(tokens)));
                }
                return Err(e).with_context(|| format!("request {}, for a summary", self.requests));
            }
        };
        let stats = request_stats(&turn, request.seed);
        let summary = turn.message.content.unwrap_or_default();
        let summary = summary.trim();
        if summary.is_empty() {
            frontend.event(Event::Notice {
                text: "the summary came back empty, so the conversation goes on as it is".into(),
            });
            // Given up on for good only by itself, which would otherwise ask after every reply.
            if asked.is_none() {
                self.uncompactable = true;
            }
            return Ok(None);
        }
        let system = self.session.messages().first().cloned().context("no system prompt")?;
        let mut first = self
            .profile
            .prompts
            .compacted
            .replace("{tokens}", &thousands(tokens))
            .replace("{summary}", summary);
        // The skills the conversation had loaded come again after the summary, which cannot hold
        // their instructions (§11.1).
        let kept = self.loaded_skills();
        for (skill, instructions) in &kept {
            first.push_str("\n\n");
            first.push_str(&self.wrap_skill(skill, instructions));
        }
        self.session.compact(tokens, summary, Some(&stats), vec![system, Message::user(first)])?;
        self.used = 0;
        // A reply cut short before is in the summary, or nowhere: the next message need not say so.
        self.unanswered = false;
        // The results that carried nested AGENTS.md files are gone: they come again when needed.
        self.tools.forget_nested();
        let again = match kept.as_slice() {
            [] => String::new(),
            [(skill, _)] => format!(", and the skill {} loaded again", skill.name),
            kept => {
                let names: Vec<&str> = kept.iter().map(|(skill, _)| skill.name.as_str()).collect();
                format!(", and the skills {} loaded again", names.join(", "))
            }
        };
        frontend.event(Event::Notice {
            text: format!(
                "compacted: the conversation goes on from a summary of {} tokens{again}",
                thousands(stats.completion_tokens.unwrap_or(0))
            ),
        });
        Ok(None)
    }

    fn has_tool(&self, name: &str) -> bool {
        self.session.header().tools.iter().any(|tool| tool.function.name == name)
    }

    /// Runs `task` calls as subagents, at most `max_parallel` at once, relays what they do to
    /// `frontend`, and returns each one's report, in the order of the calls. A call with
    /// `background` goes to the frontend to run instead, and its output says it started.
    async fn run_subagents<F: Frontend>(
        &mut self,
        calls: &[ToolCall],
        frontend: &mut F,
    ) -> Vec<ToolOutput> {
        let settings = self.profile.subagents.clone();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut outputs: Vec<Option<ToolOutput>> = calls.iter().map(|_| None).collect();
        let mut children = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            let task = match tools::task_arguments(call) {
                Ok(task) => task,
                Err(e) => {
                    outputs[i] = Some(failed(&format!("the subagent did not start: {e}")));
                    continue;
                }
            };
            let running = self.background.load(Ordering::SeqCst);
            if task.background && running >= settings.max_parallel.max(1) {
                let subagents = match running {
                    1 => "1 subagent is".to_owned(),
                    n => format!("{n} subagents are"),
                };
                outputs[i] = Some(failed(&format!(
                    "{subagents} already running in the background, the most there can be at \
                     once. Wait for a report, or run this task without background."
                )));
                continue;
            }
            let agent = match self.subagent(&task.description, &task.prompt) {
                Ok(agent) => agent,
                Err(e) => {
                    outputs[i] = Some(failed(&format!("the subagent did not start: {e:#}")));
                    continue;
                }
            };
            let agent = if task.background {
                let background = Background {
                    task: call.id.clone(),
                    label: task.description.clone(),
                    agent: Box::new(agent),
                    max_requests: settings.background_max_requests,
                    _running: Running::new(&self.background),
                };
                match frontend.background(background) {
                    Ok(()) => {
                        outputs[i] = Some(ToolOutput::text(STARTED.into(), false));
                        continue;
                    }
                    // A frontend that cannot run it in the background: it runs here, like any.
                    Err(background) => *background.agent,
                }
            } else {
                agent
            };
            let mut relay =
                Relay { task: call.id.clone(), label: task.description, sender: sender.clone() };
            relay.event(Event::Notice {
                text: format!("started; its log is {}", agent.session_log().display()),
            });
            children.push((i, relay, agent));
        }
        drop(sender);

        let limit = Semaphore::new(settings.max_parallel.max(1));
        let results = {
            let runs = children.iter_mut().map(|(_, relay, agent)| {
                let limit = &limit;
                // Boxed, since a subagent's run is this same function a level down.
                let run: Pin<Box<dyn Future<Output = Result<Outcome>> + '_>> =
                    Box::pin(async move {
                        let _turn = limit.acquire().await;
                        agent.run(relay, settings.max_requests).await
                    });
                run
            });
            let mut all = std::pin::pin!(join_all(runs));
            loop {
                tokio::select! {
                    results = &mut all => break results,
                    Some((task, label, event)) = receiver.recv() => {
                        frontend.event(Event::Subagent { task, label, event: Box::new(event) });
                    }
                }
            }
        };
        while let Ok((task, label, event)) = receiver.try_recv() {
            frontend.event(Event::Subagent { task, label, event: Box::new(event) });
        }
        for ((i, _, agent), result) in children.iter().zip(results) {
            outputs[*i] = Some(report(agent, result));
        }
        outputs.into_iter().map(|output| output.expect("every call has an output")).collect()
    }

    /// The next request: everything in the log, with the settings fixed at the start.
    fn request(&self) -> Result<ChatRequest> {
        let header = self.session.header();
        let messages = self.session.messages().to_vec();
        reply_request(&self.profile, header, messages, seed(&header.id, self.requests))
    }

    fn cut_off_message(&self) -> String {
        self.profile
            .prompts
            .cut_off
            .replace("{max_tokens}", &self.profile.request.max_tokens.to_string())
    }
}

/// The request for the reply that follows `messages` in a session: the tools, effort and mode the
/// session's header fixed (R1), and the profile's other settings. `eidra replay` builds its requests
/// here too, so a replay sends what the agent sent.
pub fn reply_request(
    profile: &Profile,
    header: &Header,
    messages: Vec<Message>,
    seed: u64,
) -> Result<ChatRequest> {
    let settings = &profile.request;
    let thinking = header.reasoning_effort != "none";
    Ok(ChatRequest {
        model: header.model.clone(),
        messages,
        tools: header.tools.clone(),
        tool_choice: Some(if header.headless { "required" } else { "auto" }.into()),
        parallel_tool_calls: settings.parallel_tool_calls,
        max_tokens: Some(settings.max_tokens),
        reasoning_effort: Some(header.reasoning_effort.clone()),
        thinking_token_budget: settings.thinking_budget.filter(|_| thinking),
        seed: Some(seed),
        session_id: Some(header.id.clone()),
        extra: profile.extra_fields()?,
    })
}

/// Whether `error` is the server refusing a prompt too long for its context, and the prompt's size
/// if it says. radiance and vLLM send OpenAI's `context_length_exceeded` with a 400, llama-server
/// its own message; radiance's carries all of them (GUIDE.md §5.8).
fn context_exceeded(error: &eidra_llm::Error) -> Option<Option<u64>> {
    let eidra_llm::Error::Status { status: 400, body, .. } = error else {
        return None;
    };
    let signs =
        ["context_length_exceeded", "maximum context length", "exceeds the available context"];
    if !signs.iter().any(|sign| body.contains(sign)) {
        return None;
    }
    let body: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    Some(body["error"]["n_prompt_tokens"].as_u64())
}

/// A request's seed: fixed by the session and the request's number, and recorded with the reply,
/// so that any request can be sent again and give the same reply (DESIGN.md §16.2).
fn seed(session: &str, request: usize) -> u64 {
    fnv1a(format!("{session}/{request}").as_bytes()) & 0x7fff_ffff
}

/// What the server reported about a reply, with the seed its request was sent with.
pub fn request_stats(turn: &AssistantTurn, seed: Option<u64>) -> RequestStats {
    let timings = turn.timings.clone().unwrap_or_default();
    let usage = turn.usage.as_ref();
    RequestStats {
        seed,
        prompt_tokens: match (timings.cache_n, timings.prompt_n) {
            (Some(cached), Some(computed)) => Some(cached + computed),
            _ => usage.map(|u| u.prompt_tokens),
        },
        cached_tokens: timings
            .cache_n
            .or_else(|| usage?.prompt_tokens_details.as_ref()?.cached_tokens),
        completion_tokens: usage.map(|u| u.completion_tokens),
        reasoning_tokens: usage.and_then(Usage::reasoning_tokens),
        prefill_ms: timings.prompt_ms,
        decode_per_second: timings.predicted_per_second,
        ttft_ms: timings.ttft_ms,
        finish_reason: turn.finish_reason.clone(),
    }
}

/// What a `task` call with `background` returns.
const STARTED: &str = "Started in the background. Its report will come as a message of its own when \
                       it ends; meanwhile, go on with the user, and leave the files it works on alone.";

/// A subagent's frontend: it passes what the subagent does to the agent that started it, and
/// declines commands that need approval, since a subagent cannot ask the user.
struct Relay {
    task: String,
    label: String,
    sender: mpsc::UnboundedSender<(String, String, Event)>,
}

impl Frontend for Relay {
    fn event(&mut self, event: Event) {
        let _ = self.sender.send((self.task.clone(), self.label.clone(), event));
    }

    async fn approve(&mut self, _approval: &Approval) -> Decision {
        declined_for_subagent()
    }
}

fn declined_for_subagent() -> Decision {
    Decision::Deny {
        note: Some(
            "A subagent cannot ask the user to approve a command. Leave this step out, and say in \
             your summary which command is needed, so that the agent that started you can run it."
                .into(),
        ),
    }
}

impl Background {
    pub fn interrupter(&self) -> Interrupter {
        self.agent.interrupter()
    }

    /// Runs the subagent to its end and returns its report. `on_event` gets what it does, as
    /// [`Event::Subagent`]. Commands that need approval are declined, as for any subagent.
    pub async fn run(mut self, on_event: impl FnMut(Event)) -> Report {
        let mut sink = Sink { task: self.task.clone(), label: self.label.clone(), on_event };
        let log = self.agent.session_log().display().to_string();
        sink.event(Event::Notice { text: format!("started in the background; its log is {log}") });
        let result = self.agent.run(&mut sink, self.max_requests).await;
        let output = report(&self.agent, result);
        Report { task: self.task, label: self.label, text: output.text, is_error: output.is_error }
    }
}

/// A background subagent's frontend.
struct Sink<F> {
    task: String,
    label: String,
    on_event: F,
}

impl<F: FnMut(Event)> Frontend for Sink<F> {
    fn event(&mut self, event: Event) {
        let (task, label) = (self.task.clone(), self.label.clone());
        (self.on_event)(Event::Subagent { task, label, event: Box::new(event) });
    }

    async fn approve(&mut self, _approval: &Approval) -> Decision {
        declined_for_subagent()
    }
}

/// What a subagent's run tells the agent that started it.
fn report(agent: &Agent, result: Result<Outcome>) -> ToolOutput {
    let unfinished = "It may have changed files: check them before relying on its work.";
    match result {
        Ok(Outcome::Finished(summary)) => ToolOutput::text(summary, false),
        Ok(Outcome::RequestLimit) => failed(&format!(
            "the subagent used all {} of its requests without finishing. {unfinished}",
            agent.requests
        )),
        Ok(Outcome::ContextLimit(tokens)) => failed(&format!(
            "the subagent's conversation reached {tokens} tokens before it finished. {unfinished}"
        )),
        Ok(Outcome::Replied) => {
            failed(&format!("the subagent stopped without a summary. {unfinished}"))
        }
        Ok(Outcome::Interrupted) => failed(&format!("the subagent was interrupted. {unfinished}")),
        Err(e) => failed(&format!("the subagent failed: {e:#}. {unfinished}")),
    }
}

fn failed(why: &str) -> ToolOutput {
    ToolOutput::text(format!("Error: {why}"), true)
}

/// A tool's result for the log: its text, and its images, each followed by its text.
fn tool_message(id: &str, text: String, images: Vec<(media::Image, String)>) -> Message {
    if images.is_empty() {
        return Message::tool(id, text);
    }
    let mut parts = vec![Part::Text { text }];
    for (image, after) in images {
        parts.push(Part::ImageUrl { image_url: ImageUrl { url: image.url } });
        if !after.is_empty() {
            parts.push(Part::Text { text: after });
        }
    }
    Message::Tool { tool_call_id: id.to_owned(), content: Content::Parts(parts) }
}

/// What heads an AGENTS.md in the system prompt, before its path.
const INSTRUCTIONS_FROM: &str = "# Instructions from ";

/// AGENTS.md from eidra's own directory and from the workspace, for the system prompt.
fn agents_md(workspace: &Path) -> String {
    let mut out = String::new();
    for path in [eidra_home(workspace).join("AGENTS.md"), workspace.join("AGENTS.md")] {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let text = text.trim();
        if !text.is_empty() {
            out.push_str(&format!("\n{INSTRUCTIONS_FROM}{}\n{text}\n", path.display()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_are_fixed_per_request_and_fit_in_31_bits() {
        assert_eq!(seed("20261026-174612-cb29", 25), seed("20261026-174612-cb29", 25));
        assert_ne!(seed("20261026-174612-cb29", 25), seed("20261026-174612-cb29", 26));
        assert!(seed("20261026-174612-cb29", 25) < 1 << 31);
    }
}
