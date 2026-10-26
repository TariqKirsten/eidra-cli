//! What an attempt's session log says about it.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub session: Option<String>,
    pub profile_hash: Option<String>,
    /// How the session ended: finished, request_limit, context_limit; none if it never ended.
    pub outcome: Option<String>,
    pub summary: Option<String>,
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub computed_tokens: u64,
    pub prefill_s: f64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub final_prompt_tokens: u64,
    /// Messages the harness sent beyond the task: nudges after replies without a tool call, and
    /// cut-off notices.
    pub nudges: u64,
    pub tool_calls: BTreeMap<String, u64>,
    pub tool_errors: BTreeMap<String, u64>,
}

impl Metrics {
    pub fn tool_error_count(&self) -> u64 {
        self.tool_errors.values().sum()
    }
}

/// Reads the session log in `sessions`. Empty metrics if the agent never started one.
pub fn read(sessions: &Path) -> Result<Metrics> {
    let log = fs::read_dir(sessions)
        .with_context(|| format!("reading {}", sessions.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
    let Some(log) = log else {
        return Ok(Metrics::default());
    };
    let text = fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;
    Ok(parse(&text))
}

fn parse(text: &str) -> Metrics {
    let mut metrics = Metrics::default();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut user_messages = 0u64;
    let string = |value: &Value| value.as_str().map(str::to_owned);
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match record["type"].as_str() {
            Some("session") => {
                metrics.session = string(&record["id"]);
                metrics.profile_hash = string(&record["profile_hash"]);
            }
            Some("end") => {
                metrics.outcome = string(&record["outcome"]);
                if metrics.outcome.as_deref() == Some("finished") {
                    metrics.summary = string(&record["detail"]);
                }
            }
            Some("message") => {
                let message = &record["message"];
                match message["role"].as_str() {
                    Some("assistant") => {
                        let stats = &record["stats"];
                        let count = |key: &str| stats[key].as_u64().unwrap_or(0);
                        let prompt = count("prompt_tokens");
                        let cached = count("cached_tokens");
                        metrics.requests += 1;
                        metrics.prompt_tokens += prompt;
                        metrics.cached_tokens += cached;
                        metrics.computed_tokens += prompt.saturating_sub(cached);
                        metrics.prefill_s += stats["prefill_ms"].as_f64().unwrap_or(0.0) / 1000.0;
                        metrics.output_tokens += count("completion_tokens");
                        metrics.reasoning_tokens += count("reasoning_tokens");
                        metrics.final_prompt_tokens = prompt;
                        for call in message["tool_calls"].as_array().into_iter().flatten() {
                            let name = call["function"]["name"].as_str().unwrap_or("?").to_owned();
                            *metrics.tool_calls.entry(name.clone()).or_default() += 1;
                            if let Some(id) = call["id"].as_str() {
                                tool_names.insert(id.to_owned(), name);
                            }
                        }
                    }
                    Some("tool")
                        if message["content"].as_str().is_some_and(|c| c.starts_with("Error:")) =>
                    {
                        let name = message["tool_call_id"]
                            .as_str()
                            .and_then(|id| tool_names.get(id))
                            .cloned()
                            .unwrap_or_else(|| "?".into());
                        *metrics.tool_errors.entry(name).or_default() += 1;
                    }
                    Some("user") => user_messages += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    metrics.nudges = user_messages.saturating_sub(1);
    metrics
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_a_session_log() {
        let log = r#"{"type":"session","id":"s1","profile_hash":"h1"}
{"type":"message","message":{"role":"system","content":"sys"}}
{"type":"message","message":{"role":"user","content":"task"}}
{"type":"message","message":{"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"write","arguments":"{}"}}]},"stats":{"prompt_tokens":1000,"cached_tokens":0,"completion_tokens":50,"reasoning_tokens":10,"prefill_ms":400.0}}
{"type":"message","message":{"role":"tool","tool_call_id":"a","content":"Error: read it first"}}
{"type":"message","message":{"role":"assistant","tool_calls":[{"id":"b","type":"function","function":{"name":"finish","arguments":"{}"}}]},"stats":{"prompt_tokens":1100,"cached_tokens":1024,"completion_tokens":20,"prefill_ms":30.0}}
{"type":"message","message":{"role":"tool","tool_call_id":"b","content":"Finished."}}
{"type":"end","outcome":"finished","detail":"done"}"#;
        let m = parse(log);
        assert_eq!((m.session.as_deref(), m.profile_hash.as_deref()), (Some("s1"), Some("h1")));
        assert_eq!((m.outcome.as_deref(), m.summary.as_deref()), (Some("finished"), Some("done")));
        assert_eq!(
            (m.requests, m.prompt_tokens, m.cached_tokens, m.computed_tokens),
            (2, 2100, 1024, 1076)
        );
        assert_eq!(
            (m.output_tokens, m.reasoning_tokens, m.final_prompt_tokens, m.nudges),
            (70, 10, 1100, 0)
        );
        assert!((m.prefill_s - 0.43).abs() < 1e-9);
        assert_eq!(m.tool_calls["write"], 1);
        assert_eq!(m.tool_errors["write"], 1);
        assert_eq!(m.tool_error_count(), 1);
    }
}
