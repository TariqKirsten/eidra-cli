//! What the agent tells a frontend, and what it asks of one.

use std::future::Future;

use serde::Serialize;

/// Something the agent did or saw, in the order it happened.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A request is going to the model.
    Request { number: usize },
    /// Reasoning text, as it streams.
    Reasoning { text: String },
    /// Reply text, as it streams.
    Content { text: String },
    /// The model's reply is complete.
    Reply { stats: RequestStats },
    /// A tool call is about to run.
    ToolCall { id: String, name: String, arguments: String },
    /// What a tool call returned.
    ToolResult { id: String, name: String, output: String, is_error: bool },
    /// The model replaced its todo list.
    Todo { list: String },
    /// Something for the user that is not part of the conversation.
    Notice { text: String },
}

/// What the server reported about one request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RequestStats {
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub prefill_ms: Option<f64>,
    pub decode_per_second: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub finish_reason: Option<String>,
}

/// A command that needs the user's go-ahead.
#[derive(Debug, Clone, Serialize)]
pub struct Approval {
    pub command: String,
    /// Why: the confirm patterns it matches, or why it could not be checked.
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run it this time.
    Once,
    /// Run it, and this exact command again for the rest of the session without asking.
    Session,
    /// Don't run it. The note goes to the model.
    Deny { note: Option<String> },
}

/// Where events go and approvals come from: the TUI, headless output, a test.
pub trait Frontend {
    fn event(&mut self, event: Event);
    fn approve(&mut self, approval: &Approval) -> impl Future<Output = Decision>;
}
