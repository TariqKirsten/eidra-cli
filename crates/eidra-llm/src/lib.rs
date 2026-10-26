//! An OpenAI-compatible chat client for eidra.
//!
//! Replies are kept byte-faithful: an assistant turn holds its reasoning, text and tool-call
//! arguments exactly as the server streamed them, so sending it back renders the same prompt and
//! the server's prefix cache keeps hitting (docs/DESIGN.md §3).

mod client;
mod sse;
mod stream;
mod types;

pub use client::{Client, Error};
pub use stream::StreamEvent;
pub use types::{
    AssistantMessage, AssistantTurn, ChatRequest, CompletionTokensDetails, Content, FunctionCall,
    FunctionDef, ImageUrl, Message, Part, PromptTokensDetails, Timings, Tool, ToolCall, Usage,
};
