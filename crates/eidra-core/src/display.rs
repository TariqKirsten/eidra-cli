//! Short descriptions of what the agent does, for the frontends: a tool call or its result in one
//! line, a request's numbers, and counts and times in a few characters.

use serde_json::Value;

use crate::event::RequestStats;

/// `1234567` as `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A count in a few characters: `950`, `10.7K`, `1.2M`.
pub fn compact(n: f64) -> String {
    match n {
        n if n >= 1e6 => format!("{:.1}M", n / 1e6),
        n if n >= 1e4 => format!("{:.0}K", n / 1e3),
        n if n >= 1e3 => format!("{:.1}K", n / 1e3),
        n => format!("{n:.0}"),
    }
}

/// Seconds as `45 s`, `3.2 min` or `1.5 h`.
pub fn duration(seconds: f64) -> String {
    match seconds {
        s if s >= 5400.0 => format!("{:.1} h", s / 3600.0),
        s if s >= 90.0 => format!("{:.1} min", s / 60.0),
        s => format!("{s:.0} s"),
    }
}

/// A request's numbers in one line.
pub fn describe_stats(stats: &RequestStats) -> String {
    let mut parts = Vec::new();
    if let Some(prompt) = stats.prompt_tokens {
        let cached = stats.cached_tokens.unwrap_or(0);
        let share = (100 * cached).checked_div(prompt).unwrap_or(0);
        parts.push(format!("{} prompt ({share}% cached)", thousands(prompt)));
    }
    if let Some(out) = stats.completion_tokens {
        match stats.reasoning_tokens {
            Some(thinking) if thinking > 0 => {
                parts.push(format!("{} out ({} thinking)", thousands(out), thousands(thinking)))
            }
            _ => parts.push(format!("{} out", thousands(out))),
        }
    }
    if let Some(ttft) = stats.ttft_ms {
        parts.push(format!("{:.1} s to first token", ttft / 1000.0));
    }
    if let Some(rate) = stats.decode_per_second {
        parts.push(format!("{rate:.0} tok/s"));
    }
    if let Some(reason) =
        stats.finish_reason.as_deref().filter(|r| *r != "tool_calls" && *r != "stop")
    {
        parts.push(format!("stopped: {reason}"));
    }
    parts.join(" · ")
}

/// A one-line description of a tool call.
pub fn describe_call(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let number = |key: &str| args.get(key).map(|v| v.to_string().trim_matches('"').to_owned());
    match name {
        "bash" => {
            let command = text("command");
            let mut line = clip(command.lines().next().unwrap_or_default(), 160);
            if command.lines().count() > 1 {
                line.push_str(" …");
            }
            let background = args.get("background").is_some_and(|b| b == true || b == "true");
            format!("$ {line}{}", if background { "  (background)" } else { "" })
        }
        "read" => {
            let range = match (number("offset"), number("limit")) {
                (None, None) => String::new(),
                (offset, limit) => format!(
                    " (from line {}{})",
                    offset.unwrap_or_else(|| "1".into()),
                    limit.map(|l| format!(", {l} lines")).unwrap_or_default()
                ),
            };
            format!("read {}{range}", text("path"))
        }
        "edit" => format!("edit {}", text("path")),
        "write" => format!("write {} ({} lines)", text("path"), text("content").lines().count()),
        "glob" => format!("glob {}", text("pattern")),
        "grep" => format!("grep {} {}", text("pattern"), text("path")).trim_end().to_owned(),
        "bash_output" | "bash_kill" => format!("{name} {}", number("id").unwrap_or_default()),
        "task" => {
            let background = args.get("background").is_some_and(|b| b == true || b == "true");
            format!(
                "task: {}{}",
                text("description"),
                if background { "  (background)" } else { "" }
            )
        }
        "todo" | "finish" => name.to_owned(),
        _ => format!("{name} {}", clip(arguments, 160)),
    }
}

/// The first line of a tool's output, and how many more there are.
pub fn summarise(output: &str) -> String {
    let lines = output.lines().count();
    let first = clip(output.lines().next().unwrap_or("(no output)"), 140);
    if lines > 1 { format!("{first}  (+{} lines)", lines - 1) } else { first }
}

/// `text` cut to `max` characters, with an ellipsis if it was longer.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(225_280), "225,280");
        assert_eq!(compact(950.0), "950");
        assert_eq!(compact(10_696.0), "11K");
        assert_eq!(compact(1_234.0), "1.2K");
        assert_eq!(compact(1_200_000.0), "1.2M");
        assert_eq!(duration(66.2), "66 s");
        assert_eq!(duration(600.0), "10.0 min");
        assert_eq!(duration(7200.0), "2.0 h");
    }

    #[test]
    fn describes_calls_in_one_line() {
        assert_eq!(
            describe_call("bash", r#"{"command": "cd x && make\nmake test"}"#),
            "$ cd x && make …"
        );
        assert_eq!(
            describe_call("bash", r#"{"command": "serve", "background": true}"#),
            "$ serve  (background)"
        );
        assert_eq!(
            describe_call("read", r#"{"path": "a.py", "offset": 40}"#),
            "read a.py (from line 40)"
        );
        assert_eq!(
            describe_call("write", r#"{"path": "a.py", "content": "x\ny\n"}"#),
            "write a.py (2 lines)"
        );
        assert_eq!(describe_call("grep", r#"{"pattern": "def main"}"#), "grep def main");
        assert_eq!(
            describe_call("task", r#"{"description": "find the parser", "prompt": "…"}"#),
            "task: find the parser"
        );
        assert_eq!(
            describe_call("task", r#"{"description": "build it", "background": true}"#),
            "task: build it  (background)"
        );
    }

    #[test]
    fn summarises_output_and_stats() {
        assert_eq!(summarise("one\ntwo\nthree"), "one  (+2 lines)");
        let stats = RequestStats {
            prompt_tokens: Some(12_000),
            cached_tokens: Some(9_000),
            completion_tokens: Some(300),
            reasoning_tokens: Some(120),
            ttft_ms: Some(1_250.0),
            decode_per_second: Some(118.4),
            finish_reason: Some("tool_calls".into()),
            ..Default::default()
        };
        assert_eq!(
            describe_stats(&stats),
            "12,000 prompt (75% cached) · 300 out (120 thinking) · 1.2 s to first token · 118 tok/s"
        );
    }
}
