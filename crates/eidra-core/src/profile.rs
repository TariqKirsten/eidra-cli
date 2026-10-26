//! Model profiles: everything model-specific, kept in files so that tuning means editing them
//! rather than recompiling (docs/DESIGN.md §4.3).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::text::fnv1a;

const BUILTIN_TOML: &str = include_str!("../../../profiles/qwen3.8-radiance/profile.toml");
const BUILTIN_SYSTEM: &str = include_str!("../../../profiles/qwen3.8-radiance/system.md");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// The environment variable that holds the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub request: RequestSettings,
    #[serde(default)]
    pub context: ContextSettings,
    pub tools: ToolSettings,
    #[serde(default)]
    pub confirm: ConfirmSettings,
    #[serde(default)]
    pub subagents: SubagentSettings,
    pub prompts: Prompts,
    /// The system prompt template: system.md beside profile.toml.
    #[serde(skip)]
    pub system_template: String,
    /// A hash of both files, recorded in every session that uses the profile.
    #[serde(skip)]
    pub hash: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSettings {
    pub max_tokens: u32,
    /// Fixed for a session: changing it rewrites the start of the prompt (DESIGN.md §3, R2).
    pub reasoning_effort: String,
    #[serde(default)]
    pub thinking_budget: Option<u32>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    /// Other request fields, such as sampling settings. Unset ones use the server's defaults.
    #[serde(default)]
    pub extra: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSettings {
    /// Prompt tokens past which a session stops (M1) or compacts (M6).
    pub limit: u64,
}

impl Default for ContextSettings {
    fn default() -> Self {
        ContextSettings { limit: 160_000 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSettings {
    pub enabled: Vec<String>,
    /// Tools added in interactive sessions, where the user can go on talking to the agent while a
    /// subagent works in the background.
    #[serde(default)]
    pub interactive: Vec<String>,
    /// Characters of one tool result the model sees; longer output is cut and saved whole.
    pub output_limit: usize,
    /// Replacement descriptions, by tool name.
    #[serde(default)]
    pub descriptions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmSettings {
    /// Commands that need the user's go-ahead (docs/DESIGN.md §7).
    #[serde(default)]
    pub commands: Vec<String>,
}

/// Subagents, which the `task` tool starts (docs/DESIGN.md §5.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentSettings {
    /// Subagents running at once. Radiance serves four requests at a time.
    pub max_parallel: usize,
    /// Requests each subagent may make before it is stopped.
    pub max_requests: usize,
    /// The same for a subagent in the background: a whole task, done while the user talks to the
    /// agent, rather than a piece of a turn.
    #[serde(default = "background_max_requests")]
    pub background_max_requests: usize,
    /// The tools a subagent gets; unset, the main agent's, without `task`.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

impl Default for SubagentSettings {
    fn default() -> Self {
        SubagentSettings {
            max_parallel: 3,
            max_requests: 40,
            background_max_requests: background_max_requests(),
            tools: None,
        }
    }
}

fn background_max_requests() -> usize {
    150
}

/// What the model is told beyond the system prompt template.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prompts {
    /// Replaces `{{mode}}` in an unattended session.
    pub headless: String,
    /// Replaces `{{mode}}` when a person is in the loop.
    pub interactive: String,
    /// Replaces `{{mode}}` for a subagent.
    pub subagent: String,
    /// Sent when a reply stops at max_tokens; `{max_tokens}` is replaced.
    pub cut_off: String,
    /// Sent in an unattended session when a reply calls no tool.
    pub no_tool_call: String,
    /// Starts the user's next message after they stopped a reply before it was finished.
    pub interrupted: String,
    /// Brings a background subagent's report; `{task}` is its description, `{report}` the report.
    pub task_report: String,
}

impl Profile {
    /// The profile built into the binary: Qwen3.8 Flash-Next on radiance.
    pub fn builtin() -> Result<Self> {
        Self::parse(BUILTIN_TOML, BUILTIN_SYSTEM).context("the built-in profile")
    }

    /// A profile directory holding profile.toml and system.md.
    pub fn load(dir: &Path) -> Result<Self> {
        let read = |name: &str| {
            let path = dir.join(name);
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
        };
        Self::parse(&read("profile.toml")?, &read("system.md")?)
            .with_context(|| format!("profile {}", dir.display()))
    }

    fn parse(toml_text: &str, system: &str) -> Result<Self> {
        let mut profile: Profile = toml::from_str(toml_text).context("profile.toml")?;
        profile.system_template = system.to_owned();
        let hash = fnv1a(toml_text.as_bytes()) ^ fnv1a(system.as_bytes()).rotate_left(1);
        profile.hash = format!("{hash:016x}");
        Ok(profile)
    }

    /// The request's extra fields as JSON.
    pub fn extra_fields(&self) -> Result<Map<String, Value>> {
        self.request
            .extra
            .iter()
            .map(|(key, value)| Ok((key.clone(), serde_json::to_value(value)?)))
            .collect()
    }

    /// The system prompt, with `{{name}}` replaced by each variable's value.
    pub fn system_prompt(&self, vars: &[(&str, &str)]) -> String {
        vars.iter().fold(self.system_template.clone(), |s, (name, value)| {
            s.replace(&format!("{{{{{name}}}}}"), value)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_profile_parses() {
        let profile = Profile::builtin().unwrap();
        assert_eq!(profile.model, "qwen3.8-flash-next");
        assert_eq!(profile.request.reasoning_effort, "xhigh");
        assert!(profile.tools.enabled.iter().any(|t| t == "bash"));
        assert_eq!(profile.hash.len(), 16);
        assert!(profile.extra_fields().unwrap().is_empty());
        let prompt = profile.system_prompt(&[("workspace", "/work"), ("date", "2026-10-26")]);
        assert!(prompt.contains("Working directory: /work"));
        assert!(!prompt.contains("{{workspace}}"));
    }
}
