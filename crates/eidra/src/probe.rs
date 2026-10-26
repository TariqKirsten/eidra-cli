//! `eidra probe`: how much of each prompt the server's prefix cache reuses.
//!
//! Runs a short scripted session (list a small generated Python project, then read its modules
//! one per request) under request policies that should and should not keep the prompt
//! append-only (docs/DESIGN.md §3). For every request it reports what the cache supplied, how
//! much of the previous prompt had to be computed again, whether the rendered prompt still began
//! with the previous one (radiance's `/apply-template`), and whether that is what the policy
//! should do.

mod fixture;

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use eidra_llm::{AssistantTurn, ChatRequest, Client, Message, Usage};
use serde::Serialize;

use crate::format::thousands;
use fixture::{Fixture, MODULES, TASK};

/// radiance snapshots a conversation every 2048 tokens (and at the end of a long enough prompt),
/// so an unchanged history comes back from the cache short by less than this.
const SNAPSHOT_INTERVAL: u64 = 2048;

#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// OpenAI-compatible API root
    #[arg(long, env = "EIDRA_BASE_URL", default_value = "http://127.0.0.1:8000/v1")]
    base_url: String,

    /// Reach the server through this Unix socket instead of the network (see `eidra relay`)
    #[arg(long, env = "EIDRA_MODEL_SOCKET", value_name = "PATH")]
    model_socket: Option<std::path::PathBuf>,

    /// Model to request
    #[arg(long, env = "EIDRA_MODEL", default_value = "qwen3.8-flash-next")]
    model: String,

    /// Environment variable that holds the API key
    #[arg(long, default_value = "RADIANCE_API_KEY")]
    api_key_env: String,

    /// Variants to run, comma-separated [default: all]
    #[arg(long, value_enum, value_delimiter = ',')]
    variant: Vec<Variant>,

    /// Requests per variant
    #[arg(long, default_value_t = 6)]
    turns: usize,

    /// Approximate size of the project notes in the system prompt, in tokens
    #[arg(long, default_value_t = 4000)]
    pad_tokens: usize,

    /// Reasoning tokens allowed per request when thinking is on
    #[arg(long, default_value_t = 512)]
    thinking_budget: u32,

    /// max_tokens for each request
    #[arg(long, default_value_t = 4096)]
    max_tokens: u32,

    /// Server root that answers /apply-template [default: the API root without /v1, then
    /// llama-swap's /upstream/<model>]
    #[arg(long)]
    extras_url: Option<String>,

    /// Print the measurements as JSON instead of tables
    #[arg(long)]
    json: bool,
}

/// A request policy to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Variant {
    /// Effort medium on every request; reasoning sent back
    Baseline,
    /// Thinking off (effort none) on every other request
    ToggleNone,
    /// Effort low on every other request
    ToggleLow,
    /// Reasoning not sent back
    DropReasoning,
    /// One system message appended halfway
    MidSystem,
}

impl Variant {
    const ALL: [Variant; 5] = [
        Variant::Baseline,
        Variant::ToggleNone,
        Variant::ToggleLow,
        Variant::DropReasoning,
        Variant::MidSystem,
    ];

    fn name(self) -> &'static str {
        match self {
            Variant::Baseline => "baseline",
            Variant::ToggleNone => "toggle-none",
            Variant::ToggleLow => "toggle-low",
            Variant::DropReasoning => "drop-reasoning",
            Variant::MidSystem => "mid-system",
        }
    }

    /// What the variant does, and what should happen (docs/DESIGN.md §3).
    fn describe(self) -> (&'static str, &'static str) {
        match self {
            Variant::Baseline => (
                "effort medium on every request; reasoning sent back",
                "the rendered prompt only grows; the cache returns all of the previous prompt \
                 but its last few hundred tokens",
            ),
            Variant::ToggleNone => (
                "thinking off (effort none) on every other request",
                "the same as baseline: medium and none render the same system prompt",
            ),
            Variant::ToggleLow => (
                "effort low on every other request",
                "each switch rewrites the start of the system prompt; each effort keeps its own \
                 cached copy, so a request recomputes everything since the last one at its effort",
            ),
            Variant::DropReasoning => (
                "reasoning not sent back",
                "the history renders the same way every time, so the cache holds as in baseline",
            ),
            Variant::MidSystem => (
                "a system message appended halfway",
                "radiance merges it into the first one: that request recomputes everything after \
                 the system prompt, then the cache holds again",
            ),
        }
    }

    fn effort(self, turn: usize) -> &'static str {
        match self {
            Variant::ToggleNone if turn.is_multiple_of(2) => "none",
            Variant::ToggleLow if turn.is_multiple_of(2) => "low",
            _ => "medium",
        }
    }

    /// Whether the policy changes the start of the prompt before request `turn`.
    fn breaks_at(self, turn: usize, turns: usize) -> bool {
        match self {
            Variant::ToggleLow => turn > 1,
            Variant::MidSystem => turn == mid_turn(turns),
            _ => false,
        }
    }
}

/// The request before which mid-system adds its message: halfway, so that most of what has to
/// be recomputed is conversation rather than system prompt.
fn mid_turn(turns: usize) -> usize {
    (turns / 2 + 1).max(2)
}

/// What one request measured.
#[derive(Debug, Serialize)]
struct TurnRecord {
    turn: usize,
    effort: &'static str,
    prompt_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    computed_tokens: Option<u64>,
    /// Tokens of the previous request's prompt that the cache did not supply.
    recomputed_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    prefill_ms: Option<f64>,
    prefill_per_second: Option<f64>,
    decode_per_second: Option<f64>,
    ttft_ms: Option<f64>,
    draft_tokens: Option<u64>,
    draft_accepted: Option<u64>,
    tools: Vec<String>,
    finish_reason: Option<String>,
    template: Template,
    /// Whether the variant changes the prompt before this request.
    expected_break: bool,
    as_expected: Option<bool>,
}

/// How this request's rendered prompt compares with the previous one's.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum Template {
    /// The first request: nothing to compare with.
    First,
    /// No `/apply-template` to render with.
    Unchecked,
    /// It begins with the previous prompt.
    Stable,
    /// It parts from the previous prompt at `at_byte`.
    Changed { at_byte: usize, before: String, after: String },
}

impl Template {
    fn label(&self) -> &'static str {
        match self {
            Template::First => "-",
            Template::Unchecked => "?",
            Template::Stable => "stable",
            Template::Changed { .. } => "CHANGED",
        }
    }
}

impl TurnRecord {
    fn new(
        turn: usize,
        effort: &'static str,
        reply: &AssistantTurn,
        previous_prompt_tokens: Option<u64>,
        template: Template,
        expected_break: bool,
    ) -> Self {
        let timings = reply.timings.clone().unwrap_or_default();
        let usage = reply.usage.as_ref();
        let cached_tokens =
            timings.cache_n.or_else(|| usage?.prompt_tokens_details.as_ref()?.cached_tokens);
        let prompt_tokens = match (timings.cache_n, timings.prompt_n) {
            (Some(cached), Some(computed)) => Some(cached + computed),
            _ => usage.map(|u| u.prompt_tokens),
        };
        let computed_tokens =
            timings.prompt_n.or_else(|| prompt_tokens?.checked_sub(cached_tokens?));
        let recomputed_tokens = previous_prompt_tokens
            .zip(cached_tokens)
            .map(|(previous, cached)| previous.saturating_sub(cached));

        // The rendered prompt should change exactly where the variant changes it. Where it
        // doesn't, the cache should return the previous prompt short of less than one snapshot
        // interval.
        let rendered_ok = match &template {
            Template::Stable => Some(!expected_break),
            Template::Changed { .. } => Some(expected_break),
            Template::First | Template::Unchecked => None,
        };
        let cache_ok = match recomputed_tokens {
            Some(recomputed) if !expected_break => Some(recomputed < SNAPSHOT_INTERVAL),
            _ => None,
        };
        let as_expected = match (rendered_ok, cache_ok) {
            (None, None) => None,
            (rendered, cache) => Some(rendered.unwrap_or(true) && cache.unwrap_or(true)),
        };

        TurnRecord {
            turn,
            effort,
            prompt_tokens,
            cached_tokens,
            computed_tokens,
            recomputed_tokens,
            completion_tokens: usage.map(|u| u.completion_tokens),
            reasoning_tokens: usage.and_then(Usage::reasoning_tokens),
            prefill_ms: timings.prompt_ms,
            prefill_per_second: timings.prompt_per_second,
            decode_per_second: timings.predicted_per_second,
            ttft_ms: timings.ttft_ms,
            draft_tokens: timings.draft_n,
            draft_accepted: timings.draft_n_accepted,
            tools: reply.message.tool_calls.iter().map(|c| c.function.name.clone()).collect(),
            finish_reason: reply.finish_reason.clone(),
            template,
            expected_break,
            as_expected,
        }
    }
}

/// What one variant measured.
#[derive(Debug, Serialize)]
struct VariantReport {
    variant: Variant,
    turns: Vec<TurnRecord>,
    /// Requests whose outcome differed from what the variant should do.
    unexpected: Vec<usize>,
    /// Prompt tokens the server computed, over all requests.
    computed_tokens: u64,
    /// The part of those that had been in the previous request's prompt.
    recomputed_tokens: u64,
    /// Server time spent computing prompt tokens.
    prefill_seconds: f64,
    template_changes: usize,
    note: Option<String>,
}

impl VariantReport {
    fn new(variant: Variant, turns: Vec<TurnRecord>, note: Option<String>) -> Self {
        let unexpected =
            turns.iter().filter(|t| t.as_expected == Some(false)).map(|t| t.turn).collect();
        let computed_tokens = turns.iter().filter_map(|t| t.computed_tokens).sum();
        let recomputed_tokens = turns.iter().filter_map(|t| t.recomputed_tokens).sum();
        let prefill_seconds = turns.iter().filter_map(|t| t.prefill_ms).sum::<f64>() / 1000.0;
        let template_changes =
            turns.iter().filter(|t| matches!(t.template, Template::Changed { .. })).count();
        VariantReport {
            variant,
            turns,
            unexpected,
            computed_tokens,
            recomputed_tokens,
            prefill_seconds,
            template_changes,
            note,
        }
    }

    fn verdict(&self) -> String {
        if !self.unexpected.is_empty() {
            let turns: Vec<String> = self.unexpected.iter().map(usize::to_string).collect();
            return format!("UNEXPECTED at request {}", turns.join(", "));
        }
        if self.turns.iter().all(|t| t.as_expected.is_none()) {
            "unchecked".into()
        } else {
            "as expected".into()
        }
    }
}

pub async fn run(args: ProbeArgs) -> Result<()> {
    let key = std::env::var(&args.api_key_env).ok().filter(|k| !k.is_empty());
    if key.is_none() {
        eprintln!("note: ${} is not set, so no API key is sent", args.api_key_env);
    }
    let client = Client::new(&args.base_url, key, args.model_socket.as_deref())?;
    let models = client.models().await.context("listing the server's models")?;
    if !models.contains(&args.model) {
        eprintln!(
            "note: /models does not list {} (it may be an alias): {}",
            args.model,
            models.join(", ")
        );
    }
    let extras = find_extras(&client, &args).await;
    let fixture = Fixture::create()?;
    let variants =
        if args.variant.is_empty() { Variant::ALL.to_vec() } else { args.variant.clone() };

    if !args.json {
        println!("server {}, model {}", client.base_url(), args.model);
        match &extras {
            Some(root) => println!("rendered prompts from {root}/apply-template"),
            None => println!(
                "rendered prompts unavailable: no /apply-template found (see --extras-url)"
            ),
        }
        println!("project {}", fixture.root().display());
        println!(
            "{} requests per variant, ~{} tokens of project notes, reasoning budget {}\n",
            args.turns, args.pad_tokens, args.thinking_budget
        );
    }

    let mut reports = Vec::new();
    for variant in variants {
        let report = run_variant(&client, extras.as_deref(), &args, variant, &fixture)
            .await
            .with_context(|| format!("variant {}", variant.name()))?;
        reports.push(report);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        print_summary(&reports);
    }
    Ok(())
}

/// Where `/apply-template` is answered, if anywhere.
async fn find_extras(client: &Client, args: &ProbeArgs) -> Option<String> {
    let candidates = match &args.extras_url {
        Some(url) => vec![url.trim_end_matches('/').to_owned()],
        None => vec![
            client.root_url().to_owned(),
            format!("{}/upstream/{}", client.root_url(), args.model),
        ],
    };
    let req = ChatRequest {
        model: args.model.clone(),
        messages: vec![Message::user("hi")],
        ..Default::default()
    };
    for root in candidates {
        match client.apply_template(&root, &req).await {
            Ok(_) => return Some(root),
            Err(err) => eprintln!("note: no /apply-template at {root}: {err}"),
        }
    }
    None
}

async fn run_variant(
    client: &Client,
    extras: Option<&str>,
    args: &ProbeArgs,
    variant: Variant,
    fixture: &Fixture,
) -> Result<VariantReport> {
    let print = !args.json;
    let session = format!("eidra-probe-{}-{}", variant.name(), unique_suffix());
    if print {
        let (does, expect) = variant.describe();
        println!("{}: {does}\n  expect: {expect}", variant.name());
        print_header();
    }

    let tools = Fixture::tools();
    let mut messages =
        vec![Message::system(system_prompt(&session, args.pad_tokens)), Message::user(TASK)];
    let mut previous_rendered: Option<String> = None;
    let mut previous_prompt_tokens: Option<u64> = None;
    let mut turns = Vec::new();
    let mut note = None;

    for turn in 1..=args.turns {
        if variant == Variant::MidSystem && turn == mid_turn(args.turns) {
            messages.push(Message::system("Reminder: keep each module's description to one line."));
        }
        let effort = variant.effort(turn);
        let req = ChatRequest {
            model: args.model.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            tool_choice: Some("required".into()),
            parallel_tool_calls: Some(false),
            max_tokens: Some(args.max_tokens),
            reasoning_effort: Some(effort.into()),
            thinking_token_budget: (effort != "none").then_some(args.thinking_budget),
            session_id: Some(session.clone()),
            ..Default::default()
        };

        let rendered = match extras {
            Some(root) => {
                Some(client.apply_template(root, &req).await.context("rendering the request")?)
            }
            None => None,
        };
        let template = match (turn, &previous_rendered, &rendered) {
            (1, _, _) => Template::First,
            (_, Some(before), Some(now)) => compare(before, now),
            _ => Template::Unchecked,
        };

        let reply = client.chat(&req, |_| {}).await.with_context(|| format!("request {turn}"))?;
        let record = TurnRecord::new(
            turn,
            effort,
            &reply,
            previous_prompt_tokens,
            template,
            variant.breaks_at(turn, args.turns),
        );
        if print {
            print_turn(&record);
        }
        previous_prompt_tokens = record.prompt_tokens;
        previous_rendered = rendered;
        turns.push(record);

        let AssistantTurn { mut message, finish_reason, .. } = reply;
        let calls = message.tool_calls.clone();
        if variant == Variant::DropReasoning {
            message.reasoning_content = None;
        }
        messages.push(Message::Assistant(message));

        if finish_reason.as_deref() == Some("length") {
            note = Some(format!("request {turn} hit max_tokens; stopped"));
            break;
        }
        if calls.is_empty() {
            note = Some(format!("request {turn} made no tool call; stopped"));
            break;
        }
        let mut finished = false;
        for call in &calls {
            let outcome = fixture.run(call);
            finished |= outcome.finished;
            messages.push(Message::tool(call.id.clone(), outcome.output));
        }
        if finished {
            if turn < args.turns {
                note = Some(format!("the model called finish at request {turn}"));
            }
            break;
        }
    }

    let report = VariantReport::new(variant, turns, note);
    if print {
        print_footer(&report);
    }
    Ok(report)
}

/// The system prompt: the session id, so no two variants share a cached prefix past the tools,
/// and about `pad_tokens` tokens of project notes standing in for AGENTS.md and the rest of a
/// real harness prompt.
fn system_prompt(session: &str, pad_tokens: usize) -> String {
    let mut s = format!(
        "You are a careful software engineer. You work on the project in the current directory, \
         only through the tools.\nSession: {session}\n\n# Project notes\n\n"
    );
    let mut n = 1;
    while s.len() < pad_tokens * 4 {
        let (module, _, _) = MODULES[n % MODULES.len()];
        let note = NOTES[(n / MODULES.len()) % NOTES.len()];
        let _ = writeln!(s, "{n}. In {module}.py: {note}");
        n += 1;
    }
    s
}

const NOTES: [&str; 8] = [
    "keep functions short, and give each public one a docstring stating inputs, outputs and failures.",
    "store money as integer cents; convert to decimal strings only when formatting output.",
    "never raise on a bad row; record its number and the reason, and let the caller decide.",
    "accept any iterable of expenses and never assume it can be traversed twice.",
    "compare totals per category and per month; a missing budget means unlimited, not zero.",
    "align columns with spaces, not tabs, and show amounts with two decimal places.",
    "name options with long hyphenated words, each with a matching environment variable.",
    "test behaviour, not implementation, using the standard library only.",
];

/// Whether `now` still begins with `before`, and if not, where they part.
fn compare(before: &str, now: &str) -> Template {
    let at = before.bytes().zip(now.bytes()).take_while(|(a, b)| a == b).count();
    if at == before.len() {
        return Template::Stable;
    }
    Template::Changed { at_byte: at, before: excerpt(before, at), after: excerpt(now, at) }
}

/// The text around byte `at`, with `⟩` marking it and newlines shown as `\n`.
fn excerpt(text: &str, at: usize) -> String {
    let start = char_floor(text, at.saturating_sub(48));
    let mid = char_floor(text, at);
    let end = char_floor(text, (at + 48).min(text.len()));
    format!("…{}⟩{}…", &text[start..mid], &text[mid..end]).replace('\n', "\\n")
}

fn char_floor(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn unique_suffix() -> String {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("{millis:x}")
}

fn print_header() {
    println!(
        "  {:>3} {:<6} {:>8} {:>7} {:>8} {:>6} {:>5} {:>6} {:>8} {:>6} {:>5} {:<10} {:<8}",
        "req",
        "effort",
        "prompt",
        "cached",
        "computed",
        "recomp",
        "reuse",
        "ttft",
        "prefill",
        "decode",
        "think",
        "tool",
        "rendered"
    );
}

fn print_turn(t: &TurnRecord) {
    let mark = match t.as_expected {
        Some(true) => "ok",
        Some(false) => "UNEXPECTED",
        None => "",
    };
    println!(
        "  {:>3} {:<6} {:>8} {:>7} {:>8} {:>6} {:>5} {:>6} {:>8} {:>6} {:>5} {:<10} {:<8} {mark}",
        t.turn,
        t.effort,
        count(t.prompt_tokens),
        count(t.cached_tokens),
        count(t.computed_tokens),
        count(t.recomputed_tokens),
        percent(t.cached_tokens, t.prompt_tokens),
        seconds(t.ttft_ms),
        rate(t.prefill_per_second),
        rate(t.decode_per_second),
        count(t.reasoning_tokens),
        t.tools.join(","),
        t.template.label(),
    );
    if let Template::Changed { at_byte, before, after } = &t.template {
        println!("      the rendered prompt changed at byte {at_byte}:");
        println!("        before  {before}");
        println!("        now     {after}");
    }
}

fn print_footer(r: &VariantReport) {
    println!(
        "  => {}. Computed {} prompt tokens in {:.1} s of prefill; {} of them recomputed from earlier prompts.",
        r.verdict(),
        count(Some(r.computed_tokens)),
        r.prefill_seconds,
        count(Some(r.recomputed_tokens)),
    );
    if let Some(note) = &r.note {
        println!("     note: {note}");
    }
    println!();
}

fn print_summary(reports: &[VariantReport]) {
    println!("summary");
    println!(
        "  {:<15} {:>8} {:>8} {:>9} {:>7} {:>8}  result",
        "variant", "requests", "changed", "computed", "recomp", "prefill"
    );
    for r in reports {
        println!(
            "  {:<15} {:>8} {:>8} {:>9} {:>7} {:>7.1}s  {}",
            r.variant.name(),
            r.turns.len(),
            r.template_changes,
            count(Some(r.computed_tokens)),
            count(Some(r.recomputed_tokens)),
            r.prefill_seconds,
            r.verdict(),
        );
    }
    println!();
    println!("  changed   requests whose rendered prompt no longer began with the previous one");
    println!(
        "  recomp    tokens of the previous prompt the cache did not supply, so computed again"
    );
    println!("  computed  all prompt tokens the server computed; prefill is the time that took");
    println!("  reuse     per request: the share of the prompt the cache supplied");
}

fn count(n: Option<u64>) -> String {
    n.map_or("-".into(), thousands)
}

fn percent(part: Option<u64>, whole: Option<u64>) -> String {
    match (part, whole) {
        (Some(part), Some(whole)) if whole > 0 => {
            format!("{:.0}%", 100.0 * part as f64 / whole as f64)
        }
        _ => "-".into(),
    }
}

fn seconds(ms: Option<f64>) -> String {
    ms.map_or("-".into(), |ms| format!("{:.1}s", ms / 1000.0))
}

fn rate(per_second: Option<f64>) -> String {
    per_second.map_or("-".into(), |r| format!("{}/s", count(Some(r.round() as u64))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidra_llm::Timings;

    fn reply(cached: u64, computed: u64) -> AssistantTurn {
        AssistantTurn {
            timings: Some(Timings {
                cache_n: Some(cached),
                prompt_n: Some(computed),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn changed() -> Template {
        Template::Changed { at_byte: 19, before: String::new(), after: String::new() }
    }

    #[test]
    fn grades_on_the_rendered_prompt_and_what_the_cache_returned() {
        // Unchanged history, only the tail past the last snapshot recomputed.
        let kept =
            TurnRecord::new(2, "medium", &reply(4096, 401), Some(4426), Template::Stable, false);
        assert_eq!((kept.recomputed_tokens, kept.as_expected), (Some(330), Some(true)));

        // Unchanged history, but the cache came back empty.
        let lost =
            TurnRecord::new(2, "medium", &reply(0, 4497), Some(4426), Template::Stable, false);
        assert_eq!(lost.as_expected, Some(false));

        // A change where none was expected.
        let surprise =
            TurnRecord::new(3, "medium", &reply(4096, 509), Some(4497), changed(), false);
        assert_eq!(surprise.as_expected, Some(false));

        // An expected change is graded on the rendered prompt alone.
        let switch = TurnRecord::new(3, "medium", &reply(4096, 3536), Some(4537), changed(), true);
        assert_eq!(switch.as_expected, Some(true));

        // The first request has nothing to compare with.
        let first = TurnRecord::new(1, "medium", &reply(0, 4426), None, Template::First, false);
        assert_eq!((first.recomputed_tokens, first.as_expected), (None, None));
    }

    #[test]
    fn compare_finds_where_prompts_part() {
        assert!(matches!(compare("abc", "abcdef"), Template::Stable));
        let Template::Changed { at_byte, before, after } = compare("ab\ncd", "ab\nXd") else {
            panic!("should differ");
        };
        assert_eq!(at_byte, 3);
        assert_eq!(before, "…ab\\n⟩cd…");
        assert_eq!(after, "…ab\\n⟩Xd…");
    }

    #[test]
    fn excerpt_stays_on_char_boundaries() {
        let text = "é".repeat(60);
        let excerpt = excerpt(&text, 61); // inside a character
        assert!(excerpt.contains('⟩'));
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(count(Some(0)), "0");
        assert_eq!(count(Some(999)), "999");
        assert_eq!(count(Some(1_000)), "1,000");
        assert_eq!(count(Some(225_280)), "225,280");
        assert_eq!(count(None), "-");
    }

    #[test]
    fn only_switching_policies_expect_a_break() {
        assert!(!Variant::Baseline.breaks_at(3, 6));
        assert!(!Variant::ToggleNone.breaks_at(2, 6));
        assert!(Variant::ToggleLow.breaks_at(2, 6));
        assert!(!Variant::ToggleLow.breaks_at(1, 6));
        assert!(Variant::MidSystem.breaks_at(4, 6));
        assert!(!Variant::MidSystem.breaks_at(5, 6));
    }

    #[test]
    fn notes_reach_the_requested_size() {
        let prompt = system_prompt("s", 1000);
        assert!(prompt.len() >= 4000);
        assert!(prompt.len() < 4200);
    }
}
