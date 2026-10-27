//! The end of the context: a long conversation goes on from a summary, and a prompt the server
//! cannot fit ends the session at its limit rather than with an error every later request repeats.

mod common;

use common::{Quiet, Reply, agent, agent_with, fake_server, recording_server, scratch};
use common::{text, tool_call, usage};
use eidra_core::{Log, Outcome};
use serde_json::json;

#[tokio::test]
async fn a_prompt_the_server_cannot_fit_ends_the_session_at_its_limit() {
    let dir = scratch("context");
    // As radiance refuses it (GUIDE.md §5.8).
    let refusal = json!({"error": {
        "code": "context_length_exceeded",
        "message": "This model's maximum context length is 225280 tokens: the request (226000 \
                    tokens) exceeds the available context size (225280 tokens) and leaves \
                    nothing to generate into; shorten the prompt by at least 721 tokens",
        "n_prompt_tokens": 226000,
        "n_ctx": 225280,
    }});
    let url = fake_server(vec![Reply::Refuse(400, refusal)]).await;
    let mut agent = agent(url, &dir);
    agent.push_user("go on").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::ContextLimit(226_000));
    let log = std::fs::read_to_string(agent.session_log()).unwrap();
    assert!(log.lines().last().unwrap().contains("context_limit"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn other_refusals_are_still_errors() {
    let dir = scratch("refusal");
    let refusal = json!({"error": {"code": "invalid_request", "message": "bad tool schema"}});
    let url = fake_server(vec![Reply::Refuse(400, refusal)]).await;
    let mut agent = agent(url, &dir);
    agent.push_user("go on").unwrap();
    let error = agent.run(&mut Quiet, 5).await.unwrap_err();
    assert!(format!("{error:#}").contains("bad tool schema"), "{error:#}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_long_conversation_goes_on_from_a_summary() {
    let dir = scratch("compact");
    let (url, requests) = recording_server(vec![
        Reply::Chunks(usage(tool_call("bash", json!({"command": "true"})), 5_000)),
        Reply::Chunks(usage(text("SUMMARY: ran true; nothing is left."), 5_100)),
        Reply::Chunks(usage(text("Done."), 600)),
    ])
    .await;
    let mut agent = agent_with(url, &dir, |profile| profile.context.compact_at = Some(1_000));
    agent.push_user("run true").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    // The summary is asked for with the whole conversation and the same tools, so that the
    // server's cache holds all of it.
    assert_eq!(requests[1]["tool_choice"], "auto");
    assert_eq!(requests[1]["tools"], requests[0]["tools"]);
    let asked: Vec<&str> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(asked, ["system", "user", "assistant", "tool", "user"]);
    // The new conversation: the same system prompt, then the summary.
    let next = requests[2]["messages"].as_array().unwrap();
    assert_eq!(next.len(), 2);
    assert_eq!(next[0], requests[0]["messages"][0]);
    let first = next[1]["content"].as_str().unwrap();
    assert!(first.contains("5,010 tokens") && first.contains("SUMMARY: ran true"), "{first}");
    // The log keeps both, and says where the second begins.
    let log = Log::read(agent.session_log(), None).unwrap();
    assert_eq!(log.starts, [0, 4]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn compact_now_summarises_when_asked_keeping_what_the_user_names() {
    let dir = scratch("compact-now");
    let (url, requests) = recording_server(vec![
        Reply::Chunks(usage(text("Hello."), 1_000)),
        Reply::Chunks(usage(text("SUMMARY: said hello."), 1_050)),
        Reply::Chunks(usage(text("Hi again."), 300)),
    ])
    .await;
    let mut agent = agent(url, &dir);
    // Before the first reply there is nothing to summarise, and nothing is asked.
    assert_eq!(agent.compact_now(&mut Quiet, "").await.unwrap(), None);
    agent.push_user("hi").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);
    assert_eq!(agent.compact_now(&mut Quiet, " the greeting ").await.unwrap(), None);
    assert_eq!(agent.context_used(), 0);
    agent.push_user("and now?").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let asked = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"].clone();
    let asked = asked.as_str().unwrap();
    assert!(asked.ends_with("wants it to keep in particular: the greeting]"), "{asked}");
    // The conversation goes on from the summary, with the user's next message after it.
    let next = requests[2]["messages"].as_array().unwrap();
    let roles: Vec<&str> = next.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "user"]);
    assert!(next[1]["content"].as_str().unwrap().contains("SUMMARY: said hello."));
    assert_eq!(next[2]["content"], "and now?");
    let _ = std::fs::remove_dir_all(&dir);
}
