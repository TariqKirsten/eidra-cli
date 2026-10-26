//! Requests and replies in the OpenAI chat-completions shape.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A chat message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System { content: Content },
    User { content: Content },
    Assistant(AssistantMessage),
    Tool { tool_call_id: String, content: Content },
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Message::System { content: Content::Text(text.into()) }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Message::User { content: Content::Text(text.into()) }
    }

    pub fn tool(tool_call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Message::Tool { tool_call_id: tool_call_id.into(), content: Content::Text(text.into()) }
    }
}

/// An assistant turn, exactly as the server produced it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The reasoning, verbatim. Sent back unchanged, it renders the same prompt the model
    /// generated, so the server's prefix cache can reuse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

/// Message content: text, or a list of text and image parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<Part>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    /// An `http(s):` URL or a `data:image/…;base64,…` URL.
    pub url: String,
}

/// A call the model made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// The arguments as the server sent them: a JSON object, as text. Never re-serialised: a
    /// different key order or spacing would change the rendered prompt.
    pub arguments: String,
}

/// A function the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}

impl Tool {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Tool {
            kind: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// A chat-completions request. The client adds `stream` and `stream_options`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// `"auto"`, `"none"` or `"required"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Counts the reasoning too, so leave room for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// One of the chat template's effort levels, or `"none"` to turn reasoning off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Reasoning tokens after which the server closes the reasoning block (vLLM's name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_token_budget: Option<u32>,
    /// The sampler's seed. Radiance's RNG is stateless, so the same seed and prompt give the same
    /// reply, from the cache or not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// A label for the conversation (OpenRouter's name for it); radiance shows it on its
    /// dashboard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Any other field the server takes: sampling settings, `chat_template_kwargs`, ...
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Token counts the server reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

impl Usage {
    pub fn reasoning_tokens(&self) -> Option<u64> {
        self.completion_tokens_details.as_ref()?.reasoning_tokens
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// llama.cpp's per-request timings, which radiance and llama-server attach to every reply.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Prompt tokens the prefix cache supplied.
    pub cache_n: Option<u64>,
    /// Prompt tokens computed for this request.
    pub prompt_n: Option<u64>,
    /// Time spent computing them.
    pub prompt_ms: Option<f64>,
    pub prompt_per_second: Option<f64>,
    pub predicted_n: Option<u64>,
    pub predicted_ms: Option<f64>,
    pub predicted_per_second: Option<f64>,
    /// Speculative tokens proposed.
    pub draft_n: Option<u64>,
    /// Speculative tokens accepted.
    pub draft_n_accepted: Option<u64>,
    /// radiance's own: from the whole request received to the first token.
    pub ttft_ms: Option<f64>,
    pub total_ms: Option<f64>,
}

/// A finished assistant turn and what the server reported about it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantTurn {
    pub message: AssistantMessage,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    pub timings: Option<Timings>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_turn_serialises_in_openai_shape() {
        let msg = Message::Assistant(AssistantMessage {
            content: None,
            reasoning_content: Some("look first\n".into()),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: "{\"path\": \"a.py\"}".into(),
                },
            }],
        });
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({
                "role": "assistant",
                "reasoning_content": "look first\n",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\": \"a.py\"}"}
                }]
            })
        );
    }

    #[test]
    fn messages_round_trip() {
        let msgs = vec![
            Message::system("s"),
            Message::user("u"),
            Message::tool("call_1", "out"),
            Message::User {
                content: Content::Parts(vec![
                    Part::Text { text: "see".into() },
                    Part::ImageUrl {
                        image_url: ImageUrl { url: "data:image/png;base64,AA==".into() },
                    },
                ]),
            },
        ];
        let text = serde_json::to_string(&msgs).unwrap();
        let back: Vec<Message> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, msgs);
    }

    #[test]
    fn request_omits_unset_fields_and_flattens_extra() {
        let mut req = ChatRequest {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            ..Default::default()
        };
        req.extra.insert("top_k".into(), json!(20));
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "top_k": 20})
        );
    }
}
