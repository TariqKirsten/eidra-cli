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
    /// Subagents the agent started, and the requests they made. Their requests, tokens, tool
    /// calls and errors are in the totals above too.
    #[serde(default)]
    pub subagents: u64,
    #[serde(default)]
    pub subagent_requests: u64,
}

impl Metrics {
    pub fn tool_error_count(&self) -> u64 {
        self.tool_errors.values().sum()
    }

    /// Adds a subagent's session to this one's totals.
    fn absorb(&mut self, sub: Metrics) {
        self.subagents += 1;
        self.subagent_requests += sub.requests;
        self.requests += sub.requests;
        self.prompt_tokens += sub.prompt_tokens;
        self.cached_tokens += sub.cached_tokens;
        self.computed_tokens += sub.computed_tokens;
        self.prefill_s += sub.prefill_s;
        self.output_tokens += sub.output_tokens;
        self.reasoning_tokens += sub.reasoning_tokens;
        self.nudges += sub.nudges;
        for (name, count) in sub.tool_calls {
            *self.tool_calls.entry(name).or_default() += count;
        }
        for (name, count) in sub.tool_errors {
            *self.tool_errors.entry(name).or_default() += count;
        }
    }
}

/// Reads the session logs in `sessions`: the agent's, with its subagents' added to its totals.
/// Empty metrics if the agent never started one.
pub fn read(sessions: &Path) -> Result<Metrics> {
    let mut logs: Vec<_> = fs::read_dir(sessions)
        .with_context(|| format!("reading {}", sessions.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    // Session ids start with the time, so the first is the agent's even if two are not subagents.
    logs.sort();
    let mut main = None;
    let mut subagents = Vec::new();
    for log in logs {
        let text =
            fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;
        let (metrics, subagent) = parse(&text);
        if subagent {
            subagents.push(metrics);
        } else if main.is_none() {
            main = Some(metrics);
        }
    }
    let mut total = main.unwrap_or_default();
    for sub in subagents {
        total.absorb(sub);
    }
    Ok(total)
}

/// A session log's metrics, and whether it is a subagent's.
fn parse(text: &str) -> (Metrics, bool) {
    let mut metrics = Metrics::default();
    let mut subagent = false;
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
                subagent = record["parent"].is_string();
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
    (metrics, subagent)
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
        let (m, subagent) = parse(log);
        assert!(!subagent);
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

    #[test]
    fn adds_subagents_to_the_agents_totals() {
        let dir = std::env::temp_dir().join(format!("eidra-metrics-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let reply = |id: &str, name: &str, prompt: u64| {
            format!(
                r#"{{"type":"message","message":{{"role":"assistant","tool_calls":[{{"id":"{id}","type":"function","function":{{"name":"{name}","arguments":"{{}}"}}}}]}},"stats":{{"prompt_tokens":{prompt},"cached_tokens":0,"completion_tokens":10}}}}"#
            )
        };
        let main = [
            r#"{"type":"session","id":"20261026-100000-aaaa","profile_hash":"h"}"#.to_owned(),
            reply("a", "task", 2000),
            r#"{"type":"message","message":{"role":"tool","tool_call_id":"a","content":"found it"}}"#.into(),
            reply("b", "finish", 2100),
            r#"{"type":"end","outcome":"finished","detail":"done"}"#.into(),
        ];
        let sub = [
            r#"{"type":"session","id":"20261026-100001-bbbb","parent":"20261026-100000-aaaa"}"#.to_owned(),
            reply("c", "grep", 1500),
            r#"{"type":"message","message":{"role":"tool","tool_call_id":"c","content":"Error: bad"}}"#.into(),
            reply("d", "finish", 1600),
            r#"{"type":"end","outcome":"finished","detail":"it is in x.py"}"#.into(),
        ];
        fs::write(dir.join("20261026-100000-aaaa.jsonl"), main.join("\n")).unwrap();
        fs::write(dir.join("20261026-100001-bbbb.jsonl"), sub.join("\n")).unwrap();
        let m = read(&dir).unwrap();
        assert_eq!(m.session.as_deref(), Some("20261026-100000-aaaa"));
        assert_eq!(m.summary.as_deref(), Some("done"));
        assert_eq!((m.requests, m.subagents, m.subagent_requests), (4, 1, 2));
        assert_eq!((m.prompt_tokens, m.final_prompt_tokens), (7200, 2100));
        assert_eq!((m.tool_calls["task"], m.tool_calls["grep"], m.tool_errors["grep"]), (1, 1, 1));
        fs::remove_dir_all(&dir).unwrap();
    }
}
