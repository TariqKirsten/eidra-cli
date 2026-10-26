//! Builds an assistant turn from the chunks of a streamed chat completion.

use serde::Deserialize;
use serde_json::Value;

use crate::types::{AssistantMessage, AssistantTurn, FunctionCall, Timings, ToolCall, Usage};

/// What one chunk added to the turn, for a frontend to show as it arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Reasoning(String),
    Content(String),
    /// A tool call's name arrived.
    ToolCall {
        index: usize,
        name: String,
    },
    /// More of a tool call's arguments arrived.
    ToolArguments {
        index: usize,
        fragment: String,
    },
}

/// One `data:` payload of the stream.
#[derive(Debug, Deserialize)]
pub(crate) struct Chunk {
    #[serde(default)]
    choices: Option<Vec<Choice>>,
    // Read leniently: servers disagree on these, and an odd report should not fail a reply.
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    timings: Option<Value>,
    #[serde(default)]
    pub(crate) error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    /// Newer vLLM and OpenRouter spell it `reasoning`.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

/// Accumulates a streamed reply. Text is concatenated exactly as it arrives, so the finished
/// turn can be sent back byte for byte.
#[derive(Debug, Default)]
pub(crate) struct TurnAccumulator {
    content: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    timings: Option<Timings>,
}

impl TurnAccumulator {
    /// Adds one chunk and returns what it added.
    pub(crate) fn push(&mut self, chunk: Chunk) -> Vec<StreamEvent> {
        if let Some(usage) = chunk.usage.and_then(|v| serde_json::from_value(v).ok()) {
            self.usage = Some(usage);
        }
        if let Some(timings) = chunk.timings.and_then(|v| serde_json::from_value(v).ok()) {
            self.timings = Some(timings);
        }
        let mut events = Vec::new();
        // eidra asks for one choice; any others are ignored.
        let Some(choice) = chunk.choices.and_then(|c| c.into_iter().next()) else {
            return events;
        };
        if choice.finish_reason.is_some() {
            self.finish_reason = choice.finish_reason;
        }
        let Some(delta) = choice.delta else {
            return events;
        };
        if let Some(text) = delta.reasoning_content.or(delta.reasoning).filter(|t| !t.is_empty()) {
            self.reasoning.push_str(&text);
            events.push(StreamEvent::Reasoning(text));
        }
        if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
            self.content.push_str(&text);
            events.push(StreamEvent::Content(text));
        }
        for call in delta.tool_calls.unwrap_or_default() {
            self.push_call(call, &mut events);
        }
        events
    }

    fn push_call(&mut self, call: ToolCallDelta, events: &mut Vec<StreamEvent>) {
        // OpenAI numbers calls with `index`. Without one, a new id starts a call and anything
        // else continues the last.
        let index = match (call.index, &call.id) {
            (Some(index), _) => index,
            (None, Some(id)) if !self.calls.iter().any(|c| &c.id == id) => self.calls.len(),
            (None, _) => self.calls.len().saturating_sub(1),
        };
        if self.calls.len() <= index {
            self.calls.resize_with(index + 1, PartialCall::default);
        }
        let partial = &mut self.calls[index];
        if let Some(id) = call.id.filter(|id| !id.is_empty())
            && partial.id.is_empty()
        {
            partial.id = id;
        }
        if let Some(kind) = call.kind.filter(|k| !k.is_empty())
            && partial.kind.is_empty()
        {
            partial.kind = kind;
        }
        let Some(function) = call.function else {
            return;
        };
        // The name arrives whole in one delta; repeats are ignored.
        if let Some(name) = function.name.filter(|n| !n.is_empty())
            && partial.name.is_empty()
        {
            partial.name.clone_from(&name);
            events.push(StreamEvent::ToolCall { index, name });
        }
        if let Some(fragment) = function.arguments.filter(|a| !a.is_empty()) {
            partial.arguments.push_str(&fragment);
            events.push(StreamEvent::ToolArguments { index, fragment });
        }
    }

    /// Whether the server said why the reply ended.
    pub(crate) fn has_finished(&self) -> bool {
        self.finish_reason.is_some()
    }

    /// The finished turn.
    pub(crate) fn finish(self) -> AssistantTurn {
        let tool_calls = self
            .calls
            .into_iter()
            .enumerate()
            .map(|(i, call)| ToolCall {
                id: if call.id.is_empty() { format!("call_{i}") } else { call.id },
                kind: if call.kind.is_empty() { "function".into() } else { call.kind },
                function: FunctionCall { name: call.name, arguments: call.arguments },
            })
            .collect();
        AssistantTurn {
            message: AssistantMessage {
                content: (!self.content.is_empty()).then_some(self.content),
                reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
                tool_calls,
            },
            finish_reason: self.finish_reason,
            usage: self.usage,
            timings: self.timings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(acc: &mut TurnAccumulator, payloads: &[&str]) -> Vec<StreamEvent> {
        payloads.iter().flat_map(|p| acc.push(serde_json::from_str(p).unwrap())).collect()
    }

    #[test]
    fn builds_reasoning_and_a_tool_call_verbatim() {
        let mut acc = TurnAccumulator::default();
        let events = feed(
            &mut acc,
            &[
                r#"{"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"reasoning_content":"Read the "}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"reasoning_content":"file.\n"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_7","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":" \"src/a.py\"}"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"completion_tokens_details":{"reasoning_tokens":12}},"timings":{"cache_n":96,"prompt_n":4,"prompt_ms":3.5}}"#,
            ],
        );
        assert_eq!(events[0], StreamEvent::Reasoning("Read the ".into()));
        assert!(events.contains(&StreamEvent::ToolCall { index: 0, name: "read_file".into() }));

        let turn = acc.finish();
        assert_eq!(turn.message.reasoning_content.as_deref(), Some("Read the file.\n"));
        assert_eq!(turn.message.content, None);
        assert_eq!(turn.message.tool_calls.len(), 1);
        assert_eq!(turn.message.tool_calls[0].id, "call_7");
        assert_eq!(turn.message.tool_calls[0].function.arguments, "{\"path\": \"src/a.py\"}");
        assert_eq!(turn.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(turn.usage.as_ref().and_then(Usage::reasoning_tokens), Some(12));
        let timings = turn.timings.unwrap();
        assert_eq!((timings.cache_n, timings.prompt_n), (Some(96), Some(4)));
    }

    #[test]
    fn keeps_parallel_calls_apart_and_reads_the_reasoning_spelling() {
        let mut acc = TurnAccumulator::default();
        feed(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"reasoning":"two reads"}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{\"path\":\"x\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"read_file","arguments":"{\"path\":\"y\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{"content":"Both read."},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        let turn = acc.finish();
        assert_eq!(turn.message.reasoning_content.as_deref(), Some("two reads"));
        assert_eq!(turn.message.content.as_deref(), Some("Both read."));
        let ids: Vec<_> = turn.message.tool_calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(turn.message.tool_calls[1].function.arguments, r#"{"path":"y"}"#);
    }

    #[test]
    fn ignores_malformed_timings_instead_of_failing() {
        let mut acc = TurnAccumulator::default();
        feed(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],"timings":{"cache_n":"lots"}}"#,
            ],
        );
        let turn = acc.finish();
        assert_eq!(turn.timings, None);
        assert_eq!(turn.message.content.as_deref(), Some("ok"));
    }
}
