//! Skills (docs/DESIGN.md §11.1): the system prompt lists them, `skill` loads one, the user's
//! `/name` brings one with their message, and a compaction loads again those the conversation had.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Quiet, Reply, recording_server, scratch, text, tool_call, usage};
use eidra_core::{Agent, AgentOptions, Outcome, Profile};
use serde_json::{Value, json};

/// A skills directory in `dir`, holding these folders with their SKILL.md.
fn skills(dir: &Path, skills: &[(&str, &str)]) -> PathBuf {
    let root = dir.join("skills");
    for (folder, text) in skills {
        fs::create_dir_all(root.join(folder)).unwrap();
        fs::write(root.join(folder).join("SKILL.md"), text).unwrap();
    }
    root
}

const GREET: &str = "---\nname: greet\ndescription: Greets people by name.\n---\n\nSay hello to $ARGUMENTS, then run scripts/wave.sh.\n";
const RELEASE: &str = "---\nname: release\ndescription: Cuts a release.\ndisable-model-invocation: true\n---\nTag version $ARGUMENTS.\n";

fn agent(url: String, dir: &Path, headless: bool, skills: Option<PathBuf>) -> Agent {
    agent_with(url, dir, headless, skills, |_| {})
}

fn agent_with(
    url: String,
    dir: &Path,
    headless: bool,
    skills: Option<PathBuf>,
    change: impl FnOnce(&mut Profile),
) -> Agent {
    let mut profile = Profile::builtin().unwrap();
    profile.base_url = url;
    change(&mut profile);
    let options = AgentOptions {
        workspace: dir.join("work"),
        session_root: dir.join("sessions"),
        headless,
        confirm: vec![],
        model_socket: None,
        skills,
    };
    Agent::start(profile, options).unwrap()
}

fn tool_names(request: &Value) -> Vec<String> {
    let tools = request["tools"].as_array().unwrap();
    tools.iter().map(|t| t["function"]["name"].as_str().unwrap().to_owned()).collect()
}

fn system_prompt(request: &Value) -> String {
    request["messages"][0]["content"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn the_system_prompt_lists_the_skills_and_skill_loads_one() {
    let dir = scratch("skills-load");
    let root = skills(&dir, &[("greet", GREET), ("release", RELEASE)]);
    let (url, requests) = recording_server(vec![
        Reply::Chunks(tool_call("skill", json!({"name": "greet"}))),
        Reply::Chunks(tool_call("skill", json!({"name": "release"}))),
        Reply::Chunks(text("Done.")),
    ])
    .await;
    let mut agent = agent(url, &dir, false, Some(root.clone()));
    agent.push_user("say hello").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);

    let requests = requests.lock().unwrap();
    assert!(tool_names(&requests[0]).contains(&"skill".to_owned()));
    // Only the skills the model may load are listed, after the mode and before AGENTS.md.
    let system = system_prompt(&requests[0]);
    assert!(system.contains("\n\n# Skills\n"), "{system}");
    assert!(
        system.contains("follow its instructions:\n- greet: Greets people by name.\n"),
        "{system}"
    );
    assert!(!system.contains("- release"), "{system}");
    // The result is the instructions, in the profile's wrapping, with the folder they start from.
    let loaded = requests[1]["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(loaded["role"], "tool");
    let expected = format!(
        "[The skill greet. Its files are in {}, and paths in it are relative to that folder. \
         Follow its instructions:]\n\nSay hello to $ARGUMENTS, then run scripts/wave.sh.",
        root.join("greet").display()
    );
    assert_eq!(loaded["content"], expected.as_str());
    // A skill only the user may run is not the model's to load.
    let refused = &requests[2]["messages"].as_array().unwrap().last().unwrap()["content"];
    assert_eq!(refused, "Error: there is no skill called release; the skills are greet");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn without_skills_for_the_model_nothing_changes() {
    let dir = scratch("skills-none");
    let only_yours = skills(&dir, &[("release", RELEASE)]);
    let (url, requests) =
        recording_server(vec![Reply::Chunks(text("Hi.")), Reply::Chunks(text("Hi."))]).await;
    // The same session, but for the skill tool in the profile.
    let mut with = agent(url.clone(), &dir, false, Some(only_yours));
    let mut without = agent_with(url, &dir, false, None, |profile| {
        profile.tools.interactive.retain(|tool| tool != "skill");
    });
    for agent in [&mut with, &mut without] {
        agent.push_user("hi").unwrap();
        agent.run(&mut Quiet, 1).await.unwrap();
    }
    let requests = requests.lock().unwrap();
    assert!(!tool_names(&requests[0]).contains(&"skill".to_owned()));
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    // The prompt is as it was before skills, but for the date and the session.
    assert_eq!(system_prompt(&requests[0]), system_prompt(&requests[1]));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unattended_runs_go_without_skills() {
    let dir = scratch("skills-headless");
    let root = skills(&dir, &[("greet", GREET)]);
    let (url, requests) =
        recording_server(vec![Reply::Chunks(tool_call("finish", json!({"summary": "ok"})))]).await;
    let mut agent = agent(url, &dir, true, Some(root));
    agent.push_user("go").unwrap();
    agent.run(&mut Quiet, 1).await.unwrap();
    let requests = requests.lock().unwrap();
    assert!(!tool_names(&requests[0]).contains(&"skill".to_owned()));
    assert!(!system_prompt(&requests[0]).contains("# Skills"));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_user_invokes_a_skill_with_what_they_typed() {
    let dir = scratch("skills-invoke");
    let root = skills(&dir, &[("greet", GREET), ("release", RELEASE)]);
    let agent = agent("http://127.0.0.1:9/v1".into(), &dir, false, Some(root.clone()));
    let (text, lines) = agent.invoke_skill("release", " 1.2.0 ").unwrap();
    assert!(text.ends_with("Follow its instructions:]\n\nTag version 1.2.0."), "{text}");
    assert_eq!(lines, 1);
    assert!(text.contains(&root.join("release").display().to_string()));
    let missing = agent.invoke_skill("nope", "").unwrap_err();
    assert_eq!(missing, "there is no skill called nope");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_compaction_loads_again_the_skills_the_conversation_had() {
    let dir = scratch("skills-compact");
    let root = skills(&dir, &[("greet", GREET), ("release", RELEASE)]);
    let (url, requests) = recording_server(vec![
        Reply::Chunks(usage(tool_call("skill", json!({"name": "greet"})), 500)),
        Reply::Chunks(usage(text("Hello."), 5_000)),
        Reply::Chunks(usage(text("SUMMARY: said hello."), 5_100)),
        Reply::Chunks(usage(text("Done."), 600)),
    ])
    .await;
    let mut agent = agent_with(url, &dir, false, Some(root), |profile| {
        profile.context.compact_at = Some(1_000);
    });
    agent.push_user("say hello").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);
    // The user ran the other one themselves.
    let (release, _) = agent.invoke_skill("release", "2.0").unwrap();
    agent.push_user(&format!("/release 2.0\n\n{release}")).unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let next = requests[3]["messages"].as_array().unwrap();
    assert_eq!(next.len(), 2);
    let first = next[1]["content"].as_str().unwrap();
    assert!(first.contains("SUMMARY: said hello."), "{first}");
    // Both come again after the summary, as they are on disk.
    let greet = first.find("[The skill greet.").expect(first);
    let release = first.find("[The skill release.").expect(first);
    assert!(first.find("SUMMARY").unwrap() < greet && greet < release, "{first}");
    assert!(first.contains("Tag version $ARGUMENTS."), "{first}");
    let _ = fs::remove_dir_all(&dir);
}
