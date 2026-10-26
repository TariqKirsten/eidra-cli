//! Subagents in the background: the `task` call returns at once, the frontend runs the subagent,
//! and its report comes back to the agent that started it as a message of its own.

mod common;

use common::{Quiet, Reply, agent, agent_with, fake_server, log_messages, scratch};
use common::{text, tool_call, tool_calls};
use eidra_core::{Approval, Background, Decision, Event, Frontend, Outcome};
use serde_json::{Value, json};

/// A frontend that keeps the subagents it is given to run in the background.
#[derive(Default)]
struct Keeper {
    tasks: Vec<Background>,
}

impl Frontend for Keeper {
    fn event(&mut self, _event: Event) {}

    async fn approve(&mut self, _approval: &Approval) -> Decision {
        Decision::Deny { note: None }
    }

    fn background(&mut self, task: Background) -> Result<(), Background> {
        self.tasks.push(task);
        Ok(())
    }
}

fn task(description: &str) -> (&str, Value) {
    ("task", json!({"description": description, "prompt": "Do it.", "background": true}))
}

fn tool_results(messages: &[Value]) -> Vec<String> {
    let results = messages.iter().filter(|m| m["role"] == "tool");
    results.map(|m| m["content"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn a_background_task_returns_at_once_and_reports_later() {
    let dir = scratch("background");
    let (name, arguments) = task("build it");
    let url = fake_server(vec![
        Reply::Chunks(tool_call(name, arguments)),
        Reply::Chunks(text("Started it.")),
        Reply::Chunks(tool_call("finish", json!({"summary": "Built calc.py."}))),
    ])
    .await;
    let mut agent = agent(url, &dir);
    agent.push_user("build it in the background").unwrap();
    let mut keeper = Keeper::default();
    assert_eq!(agent.run(&mut keeper, 5).await.unwrap(), Outcome::Replied);
    let results = tool_results(&log_messages(&agent));
    assert!(results[0].starts_with("Started in the background"), "{results:?}");

    // The turn is over; now the subagent runs, and its report comes back.
    let task = keeper.tasks.pop().unwrap();
    assert_eq!(task.label, "build it");
    let mut events = Vec::new();
    let report = task.run(|event| events.push(event)).await;
    assert_eq!((report.text.as_str(), report.is_error), ("Built calc.py.", false));
    assert!(matches!(&events[0], Event::Subagent { label, .. } if label == "build it"));
    let message = agent.report_message(&report);
    assert!(message.contains("\"build it\"") && message.contains("\n\nBuilt calc.py.\n\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_background_task_has_a_request_limit_of_its_own() {
    let dir = scratch("background-requests");
    let (name, arguments) = task("build it");
    let url = fake_server(vec![
        Reply::Chunks(tool_call(name, arguments)),
        Reply::Chunks(text("Started it.")),
        Reply::Chunks(tool_call("bash", json!({"command": "true"}))),
    ])
    .await;
    let mut agent = agent_with(url, &dir, |profile| {
        profile.subagents.max_requests = 5;
        profile.subagents.background_max_requests = 1;
    });
    agent.push_user("build it in the background").unwrap();
    let mut keeper = Keeper::default();
    agent.run(&mut keeper, 5).await.unwrap();
    let report = keeper.tasks.pop().unwrap().run(|_| {}).await;
    assert!(report.is_error);
    assert!(report.text.contains("used all 1 of its requests"), "{}", report.text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_frontend_that_cannot_runs_a_background_task_in_the_call() {
    let dir = scratch("background-fallback");
    let (name, arguments) = task("build it");
    let url = fake_server(vec![
        Reply::Chunks(tool_call(name, arguments)),
        Reply::Chunks(tool_call("finish", json!({"summary": "Built calc.py."}))),
        Reply::Chunks(text("Done.")),
    ])
    .await;
    let mut agent = agent(url, &dir);
    agent.push_user("build it").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);
    assert_eq!(tool_results(&log_messages(&agent)), ["Built calc.py."]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn background_tasks_are_limited_and_a_finished_one_frees_its_place() {
    let dir = scratch("background-limit");
    let url = fake_server(vec![
        Reply::Chunks(tool_calls(&[task("one"), task("two")])),
        Reply::Chunks(text("Started one.")),
        Reply::Chunks(tool_calls(&[task("three")])),
        Reply::Chunks(text("Started three.")),
    ])
    .await;
    let mut agent = agent_with(url, &dir, |profile| profile.subagents.max_parallel = 1);
    agent.push_user("start two").unwrap();
    let mut keeper = Keeper::default();
    agent.run(&mut keeper, 5).await.unwrap();
    let results = tool_results(&log_messages(&agent));
    assert!(results[0].starts_with("Started in the background"), "{results:?}");
    assert!(results[1].starts_with("Error: 1 subagent is already running"), "{results:?}");
    assert_eq!(keeper.tasks.len(), 1);

    // Once it is gone, another can start.
    keeper.tasks.clear();
    agent.push_user("start another").unwrap();
    agent.run(&mut keeper, 5).await.unwrap();
    let results = tool_results(&log_messages(&agent));
    assert!(results[2].starts_with("Started in the background"), "{results:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
