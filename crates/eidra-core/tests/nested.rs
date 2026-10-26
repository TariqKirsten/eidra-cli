//! Nested AGENTS.md files: one joins the first tool result that touches its directory, once.

mod common;

use std::fs;

use common::{Quiet, Reply, agent, fake_server, log_messages, scratch, text, tool_call};
use eidra_core::Outcome;
use serde_json::json;

#[tokio::test]
async fn a_nested_agents_md_comes_with_the_first_result_in_its_directory() {
    let dir = scratch("nested");
    let work = dir.join("work");
    fs::create_dir_all(work.join("api")).unwrap();
    fs::write(work.join("api/AGENTS.md"), "Run the API tests with make api-test.\n").unwrap();
    fs::write(work.join("api/a.py"), "A = 1\n").unwrap();
    fs::write(work.join("api/b.py"), "B = 2\n").unwrap();
    let url = fake_server(vec![
        Reply::Chunks(tool_call("read", json!({"path": "api/a.py"}))),
        Reply::Chunks(tool_call("read", json!({"path": "api/b.py"}))),
        Reply::Chunks(tool_call("bash", json!({"command": "cd api"}))),
        Reply::Chunks(text("Done.")),
    ])
    .await;
    let mut agent = agent(url, &dir);
    agent.push_user("read the API").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);

    let results: Vec<String> = log_messages(&agent)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap().to_owned())
        .collect();
    let note = "# Instructions from api/AGENTS.md, for api/\nRun the API tests with make api-test.";
    assert!(results[0].contains("A = 1") && results[0].ends_with(note), "{}", results[0]);
    assert!(results[1].contains("B = 2") && !results[1].contains("AGENTS.md"), "{}", results[1]);
    assert!(!results[2].contains("AGENTS.md"), "{}", results[2]);
    let _ = fs::remove_dir_all(&dir);
}
