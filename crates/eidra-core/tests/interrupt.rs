//! Interrupting a run, against a fake server that streams scripted replies.

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::{Quiet, Reply, agent, fake_server, log_messages, scratch, tool_call};
use eidra_core::{Outcome, Profile};
use serde_json::json;

#[tokio::test]
async fn an_interrupted_command_stops_with_everything_it_started() {
    let dir = scratch("tool");
    // The command starts a background process in its group, which would leave a marker after a
    // second if it outlived the interrupt.
    let command = "(sleep 1; touch marker) & sleep 30";
    let url =
        fake_server(vec![Reply::Chunks(tool_call("bash", json!({"command": command})))]).await;
    let mut agent = agent(url, &dir);
    agent.push_user("run it").unwrap();
    let interrupter = agent.interrupter();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        interrupter.interrupt();
    });

    let started = Instant::now();
    let outcome = agent.run(&mut Quiet, 5).await.unwrap();
    assert_eq!(outcome, Outcome::Interrupted);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());

    let messages = log_messages(&agent);
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "tool");
    assert!(last["content"].as_str().unwrap().contains("interrupted"), "{last}");

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!dir.join("work/marker").exists(), "the background process outlived the interrupt");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_interrupted_reply_leaves_nothing_in_the_log() {
    let dir = scratch("stream");
    let reasoning = json!({"choices": [{"index": 0, "delta": {"reasoning_content": "Let me"}}]});
    let url = fake_server(vec![Reply::Stall(vec![reasoning])]).await;
    let mut agent = agent(url, &dir);
    agent.push_user("think about it").unwrap();
    let interrupter = agent.interrupter();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        interrupter.interrupt();
    });

    let outcome = agent.run(&mut Quiet, 5).await.unwrap();
    assert_eq!(outcome, Outcome::Interrupted);
    let roles: Vec<String> =
        log_messages(&agent).iter().map(|m| m["role"].as_str().unwrap().to_owned()).collect();
    assert_eq!(roles, ["system", "user"]);

    // The next message says what happened; the one after it does not.
    let note = Profile::builtin().unwrap().prompts.interrupted;
    agent.push_user("something else").unwrap();
    agent.push_user("and more").unwrap();
    let messages = log_messages(&agent);
    let contents: Vec<&str> =
        messages[2..].iter().map(|m| m["content"].as_str().unwrap()).collect();
    assert_eq!(contents, [format!("{note}\n\nsomething else").as_str(), "and more"]);
    let _ = fs::remove_dir_all(&dir);
}
