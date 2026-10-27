//! Going on with a session from its log, as `--continue` and `--resume` do.

mod common;

use std::time::Duration;

use common::{Quiet, Reply, agent, fake_server, recording_server, scratch};
use common::{text, tool_call, usage};
use eidra_core::{Agent, AgentOptions, Log, Outcome, Profile};
use serde_json::json;

fn options(dir: &std::path::Path, headless: bool) -> AgentOptions {
    AgentOptions {
        workspace: dir.join("work"),
        session_root: dir.join("sessions"),
        headless,
        confirm: vec![],
        model_socket: None,
    }
}

#[test]
fn a_session_knows_the_agents_md_files_it_began_with_and_keeps_them() {
    let dir = scratch("instructions");
    std::fs::write(dir.join("work/AGENTS.md"), "Use tabs.\n").unwrap();
    let mut first = agent("http://127.0.0.1:9/v1".into(), &dir);
    let read = first.instructions();
    // The user's own, in ~/.eidra, may come first.
    let ours = dir.join("work").canonicalize().unwrap().join("AGENTS.md");
    assert_eq!(read.last(), Some(&ours));
    first.push_user("hi").unwrap();
    let log = first.session_log().to_path_buf();
    drop(first);

    // Removed since: the session goes on with what it read when it began.
    std::fs::remove_file(dir.join("work/AGENTS.md")).unwrap();
    let resumed = Agent::resume(Profile::builtin().unwrap(), options(&dir, false), &log).unwrap();
    assert_eq!(resumed.instructions(), read);
}

#[tokio::test]
async fn a_session_goes_on_where_it_stopped() {
    let dir = scratch("resume");
    let url = fake_server(vec![
        Reply::Chunks(usage(text("Hello."), 1_000)),
        Reply::Chunks(usage(tool_call("bash", json!({"command": "sleep 30"})), 1_100)),
    ])
    .await;
    let mut first = agent(url, &dir);
    first.push_user("hi").unwrap();
    assert_eq!(first.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);
    first.push_user("wait a while").unwrap();
    // eidra stops while the command runs: the call has no result in the log.
    let mut quiet = Quiet;
    let stopped = tokio::time::timeout(Duration::from_millis(500), first.run(&mut quiet, 5));
    assert!(stopped.await.is_err());
    let log = first.session_log().to_path_buf();
    drop(first);

    let (url, requests) = recording_server(vec![Reply::Chunks(usage(text("Back."), 1_300))]).await;
    let mut profile = Profile::builtin().unwrap();
    profile.base_url = url;
    let mut resumed = Agent::resume(profile, options(&dir, false), &log).unwrap();
    assert_eq!(resumed.session_log(), log);
    assert!(!resumed.profile_changed());
    resumed.push_user("are you there?").unwrap();
    assert_eq!(resumed.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);

    let requests = requests.lock().unwrap();
    let messages = requests[0]["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user", "assistant", "tool", "user"]);
    let result = messages[5]["content"].as_str().unwrap();
    assert!(result.starts_with("Error: not run, because eidra stopped"), "{result}");
    // The turn was cut short, and the next message says so.
    let next = messages[6]["content"].as_str().unwrap();
    assert!(next.starts_with("[The user interrupted you") && next.ends_with("are you there?"));

    // The same log goes on, with a mark where it was taken up again and a seed not used before.
    let read = Log::read(&log).unwrap();
    let seeds: Vec<u64> = read.messages.iter().filter_map(|(_, s)| s.as_ref()?.seed).collect();
    assert_eq!(seeds.len(), 3);
    assert!(seeds[2] != seeds[0] && seeds[2] != seeds[1]);
    assert!(std::fs::read_to_string(&log).unwrap().contains(r#""type":"resumed""#));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_finished_reply_needs_no_note_and_the_kind_of_session_is_kept() {
    let dir = scratch("resume-kind");
    let url = fake_server(vec![Reply::Chunks(usage(text("Hello."), 1_000))]).await;
    let mut first = agent(url.clone(), &dir);
    first.push_user("hi").unwrap();
    first.run(&mut Quiet, 5).await.unwrap();
    let log = first.session_log().to_path_buf();
    drop(first);

    let (url, requests) = recording_server(vec![Reply::Chunks(text("Yes."))]).await;
    let mut profile = Profile::builtin().unwrap();
    profile.base_url = url;
    let unattended = Agent::resume(profile.clone(), options(&dir, true), &log);
    let why = unattended.err().unwrap().to_string();
    assert!(why.contains("ran interactively"), "{why}");
    let mut resumed = Agent::resume(profile, options(&dir, false), &log).unwrap();
    resumed.push_user("again").unwrap();
    resumed.run(&mut Quiet, 5).await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0]["messages"].as_array().unwrap().last().unwrap()["content"], "again");
    let _ = std::fs::remove_dir_all(&dir);
}
