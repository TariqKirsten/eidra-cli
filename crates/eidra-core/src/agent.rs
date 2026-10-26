//! The agent loop (docs/DESIGN.md §4): build a request from the session log, stream the reply,
//! run its tool calls, append everything to the log, repeat. The log only grows, and the system
//! prompt, tools and effort are fixed when the session starts, so every request begins with the
//! one before it (§3).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use eidra_llm::{AssistantTurn, ChatRequest, Client, Message, StreamEvent, Usage};

use crate::confirm::ConfirmPolicy;
use crate::event::{Event, Frontend, RequestStats};
use crate::profile::Profile;
use crate::session::{Header, Session};
use crate::text::utc_datetime;
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
}

impl Agent {
    pub fn start(profile: Profile, options: AgentOptions) -> Result<Self> {
        let workspace = fs::canonicalize(&options.workspace)
            .with_context(|| format!("the working directory {}", options.workspace.display()))?;
        let api_key = profile
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|key| !key.is_empty());
        let client = Client::new(&profile.base_url, api_key)?;

        let definitions = tools::definitions(&profile, options.headless)?;
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
        };
        let mut session = Session::create(&options.session_root, header)?;
        let tools = Toolbox::new(&profile, &workspace, session.dir(), definitions)?;

        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let (year, month, day, ..) = utc_datetime(secs);
        let mode =
            if options.headless { &profile.prompts.headless } else { &profile.prompts.interactive };
        let system = profile.system_prompt(&[
            ("workspace", &workspace.display().to_string()),
            ("date", &format!("{year:04}-{month:02}-{day:02}")),
            ("mode", mode),
            ("agents", &agents_md(&workspace)),
        ]);
        session.push(Message::system(system), None)?;

        let mut patterns = profile.confirm.commands.clone();
        patterns.extend(options.confirm);
        Ok(Agent {
            client,
            confirm: ConfirmPolicy::new(&patterns),
            profile,
            workspace,
            session,
            tools,
            headless: options.headless,
            requests: 0,
        })
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

    /// The session's log file.
    pub fn session_log(&self) -> &Path {
        self.session.path()
    }

    pub fn push_user(&mut self, text: &str) -> Result<()> {
        self.session.push(Message::user(text), None)
    }

    /// Requests and runs tools until the model finishes or replies, or a limit is reached.
    pub async fn run<F: Frontend>(
        &mut self,
        frontend: &mut F,
        max_requests: usize,
    ) -> Result<Outcome> {
        for _ in 0..max_requests {
            self.requests += 1;
            frontend.event(Event::Request { number: self.requests });
            let request = self.request()?;
            let turn = self
                .client
                .chat(&request, |event| match event {
                    StreamEvent::Reasoning(text) => frontend.event(Event::Reasoning { text }),
                    StreamEvent::Content(text) => frontend.event(Event::Content { text }),
                    StreamEvent::ToolCall { .. } | StreamEvent::ToolArguments { .. } => {}
                })
                .await
                .with_context(|| format!("request {}", self.requests))?;
            let stats = stats(&turn);
            frontend.event(Event::Reply { stats: stats.clone() });
            let AssistantTurn { message, finish_reason, .. } = turn;
            let calls = message.tool_calls.clone();
            self.session.push(Message::Assistant(message), Some(&stats))?;

            let used = stats.prompt_tokens.unwrap_or(0) + stats.completion_tokens.unwrap_or(0);
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
            for (i, call) in calls.iter().enumerate() {
                frontend.event(Event::ToolCall {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                });
                // A reply cut off at max_tokens may end in a call that was cut off too.
                let output = if cut_off && i + 1 == calls.len() {
                    ToolOutput {
                        text: format!("Error: {}", self.cut_off_message()),
                        is_error: true,
                        finish: None,
                    }
                } else {
                    self.tools.call(call, &mut self.confirm, frontend).await
                };
                frontend.event(Event::ToolResult {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    output: output.text.clone(),
                    is_error: output.is_error,
                });
                self.session.push(Message::tool(call.id.clone(), output.text), None)?;
                if output.finish.is_some() {
                    finished = output.finish;
                }
            }
            if let Some(summary) = finished {
                self.session.end("finished", &summary)?;
                return Ok(Outcome::Finished(summary));
            }
        }
        self.session.end("request_limit", "")?;
        Ok(Outcome::RequestLimit)
    }

    /// The next request: everything in the log, with the settings fixed at the start.
    fn request(&self) -> Result<ChatRequest> {
        let settings = &self.profile.request;
        let thinking = settings.reasoning_effort != "none";
        Ok(ChatRequest {
            model: self.profile.model.clone(),
            messages: self.session.messages().to_vec(),
            tools: self.tools.definitions().to_vec(),
            tool_choice: Some(if self.headless { "required" } else { "auto" }.into()),
            parallel_tool_calls: settings.parallel_tool_calls,
            max_tokens: Some(settings.max_tokens),
            reasoning_effort: Some(settings.reasoning_effort.clone()),
            thinking_token_budget: settings.thinking_budget.filter(|_| thinking),
            session_id: Some(self.session.header().id.clone()),
            extra: self.profile.extra_fields()?,
        })
    }

    fn cut_off_message(&self) -> String {
        self.profile
            .prompts
            .cut_off
            .replace("{max_tokens}", &self.profile.request.max_tokens.to_string())
    }
}

fn stats(turn: &AssistantTurn) -> RequestStats {
    let timings = turn.timings.clone().unwrap_or_default();
    let usage = turn.usage.as_ref();
    RequestStats {
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

/// AGENTS.md from the user's config directory and from the workspace, for the system prompt.
fn agents_md(workspace: &Path) -> String {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let mut out = String::new();
    for path in config
        .map(|dir| dir.join("eidra/AGENTS.md"))
        .into_iter()
        .chain([workspace.join("AGENTS.md")])
    {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let text = text.trim();
        if !text.is_empty() {
            out.push_str(&format!("\n# Instructions from {}\n{text}\n", path.display()));
        }
    }
    out
}
