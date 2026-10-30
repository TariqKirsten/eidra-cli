//! `eidra replay`: sends one request of a recorded session again (docs/DESIGN.md §16.2).
//!
//! Radiance samples deterministically for a seed and a prompt, from its cache or not. A replay
//! with the seed recorded with a reply should give that reply again; one that differs means the
//! server computed something else. Other seeds show what else the model might have done there.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use clap::Args;
use eidra_core::{Log, Profile, fnv1a, reply_request, request_stats};
use eidra_llm::{AssistantMessage, Client, Message};
use serde_json::json;

use eidra_core::display::{describe_call, describe_stats};

#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// The session's log
    #[arg(value_name = "SESSION.jsonl")]
    session: PathBuf,

    /// The reply to send again, counting from 1
    #[arg(long, value_name = "N")]
    reply: usize,

    /// A seed to sample with; repeatable [default: the seed recorded with the reply]
    #[arg(long = "seed", value_name = "SEED")]
    seeds: Vec<u64>,

    /// Sample this many more times, with new seeds
    #[arg(long, default_value_t = 0, value_name = "N")]
    samples: usize,

    /// Empty the server's prefix cache before each sample, so that the whole prompt is computed
    /// again. The cache is the server's, so this costs everyone using it a recompute
    #[arg(long)]
    cold: bool,

    /// The profile the session ran with, as a directory [default: the built-in profile]
    #[arg(long, value_name = "DIR")]
    profile: Option<PathBuf>,

    /// Reach the server through this Unix socket instead of the network (see `eidra relay`)
    #[arg(long, env = "EIDRA_MODEL_SOCKET", value_name = "PATH")]
    model_socket: Option<PathBuf>,

    /// For an https server with a certificate of its own: the certificate's SHA-256 [default: the
    /// profile's]
    #[arg(long, env = "EIDRA_TLS_PIN", value_name = "SHA256")]
    tls_pin: Option<String>,
}

pub async fn run(args: ReplayArgs) -> Result<()> {
    let log = Log::read(&args.session)?;
    let header = &log.header;
    let profile = match &args.profile {
        Some(dir) => Profile::load(dir)?,
        None => Profile::builtin()?,
    };
    if profile.hash != header.profile_hash {
        eprintln!(
            "note: the session ran with profile {} ({}), not {} ({}); settings outside the \
             session's header may differ",
            header.profile,
            &header.profile_hash[..8.min(header.profile_hash.len())],
            profile.name,
            &profile.hash[..8],
        );
    }

    // The reply, and the messages its request held.
    let replies = log.messages.iter().filter(|(m, _)| matches!(m, Message::Assistant(_))).count();
    let Some(index) = log
        .messages
        .iter()
        .enumerate()
        .filter(|(_, (m, _))| matches!(m, Message::Assistant(_)))
        .nth(args.reply.wrapping_sub(1))
        .map(|(i, _)| i)
    else {
        bail!("the session has {replies} replies; there is no reply {}", args.reply);
    };
    let (Message::Assistant(recorded), stats) = &log.messages[index] else {
        unreachable!("index points at a reply");
    };
    let recorded_seed = stats.as_ref().and_then(|s| s.seed);
    // The reply's own conversation: a compaction starts a new one.
    let start = log.start_of(index);
    let messages: Vec<Message> =
        log.messages[start..index].iter().map(|(m, _)| m.clone()).collect();

    let mut seeds = args.seeds.clone();
    if seeds.is_empty() && args.samples == 0 {
        match recorded_seed {
            Some(seed) => seeds.push(seed),
            None => bail!(
                "reply {} has no recorded seed (the session is older than seeds); pass --seed or \
                 --samples",
                args.reply
            ),
        }
    }
    seeds.extend(
        (0..args.samples)
            .map(|i| fnv1a(format!("{}/{}/{i}", header.id, args.reply).as_bytes()) & 0x7fff_ffff),
    );

    let api_key = profile.api_key_env.as_deref().and_then(|name| std::env::var(name).ok());
    let api_key = api_key.filter(|key| !key.is_empty());
    let pin = args.tls_pin.as_deref().or(profile.tls_pin.as_deref());
    let client = Client::new(&header.base_url, api_key, args.model_socket.as_deref(), pin)?;
    let out = args.session.with_extension("").join("replays");
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;

    println!(
        "session {} reply {} of {replies}, after {} messages",
        header.id,
        args.reply,
        messages.len()
    );
    if let Some(stats) = stats {
        println!("  recorded  {}{}", describe_stats(stats), calls(recorded));
    }
    for seed in seeds {
        if args.cold {
            reset_cache(&client, &header.model).await?;
        }
        let mut request = reply_request(&profile, header, messages.clone(), seed)?;
        request.session_id = Some(format!("{}-replay", header.id));
        let turn = client
            .chat(&request, |_| {})
            .await
            .with_context(|| format!("replaying with seed {seed}"))?;
        let stats = request_stats(&turn, Some(seed));
        let verdict =
            (Some(seed) == recorded_seed).then(|| match difference(recorded, &turn.message) {
                None => "same as recorded".to_owned(),
                Some(how) => format!("DIFFERS from the recorded reply: {how}"),
            });
        println!(
            "  seed {seed:<10} {}{}{}",
            describe_stats(&stats),
            calls(&turn.message),
            verdict.as_deref().map_or(String::new(), |v| format!("  · {v}"))
        );
        let sample = json!({
            "seed": seed,
            "cold": args.cold,
            "same_as_recorded": verdict.as_ref().map(|v| v == "same as recorded"),
            "message": turn.message,
            "finish_reason": turn.finish_reason,
            "stats": stats,
        });
        let path = out.join(format!("reply{}-seed{seed}.json", args.reply));
        fs::write(&path, serde_json::to_string_pretty(&sample)? + "\n")?;
    }
    println!("  samples saved in {}", out.display());
    Ok(())
}

/// The calls a reply made, as `→ name args; name args`.
fn calls(message: &AssistantMessage) -> String {
    let calls: Vec<String> = message
        .tool_calls
        .iter()
        .map(|c| describe_call(&c.function.name, &c.function.arguments))
        .collect();
    if calls.is_empty() { String::new() } else { format!("  → {}", calls.join("; ")) }
}

/// How a replayed reply differs from the recorded one, or `None` if it is the same. Call ids are
/// left out, since the server makes them up.
fn difference(recorded: &AssistantMessage, replayed: &AssistantMessage) -> Option<String> {
    let text = |m: &AssistantMessage, reasoning: bool| {
        let field = if reasoning { &m.reasoning_content } else { &m.content };
        field.clone().unwrap_or_default()
    };
    for (name, reasoning) in [("reasoning", true), ("text", false)] {
        let (a, b) = (text(recorded, reasoning), text(replayed, reasoning));
        if a != b {
            let same = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
            return Some(format!("its {name} parts at character {same}"));
        }
    }
    let calls = |m: &AssistantMessage| {
        m.tool_calls
            .iter()
            .map(|c| (c.function.name.clone(), c.function.arguments.clone()))
            .collect::<Vec<_>>()
    };
    (calls(recorded) != calls(replayed)).then(|| "its tool calls".to_owned())
}

/// Empties the prefix cache of the server behind `client`: radiance itself, or radiance behind
/// llama-swap.
async fn reset_cache(client: &Client, model: &str) -> Result<()> {
    let root = client.root_url();
    let mut errors = Vec::new();
    for candidate in [root.to_owned(), format!("{root}/upstream/{model}")] {
        match client.reset_prefix_cache(&candidate).await {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(e.to_string()),
        }
    }
    bail!("could not empty the server's prefix cache:\n  {}", errors.join("\n  "))
}
