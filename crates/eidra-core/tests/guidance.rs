//! What the harness tells the model beyond the tools' own results: an edit that changes the
//! project's tests, a finish that leaves them changed, a budget nearly spent, a timeout under
//! another name, and no network.

mod common;

use std::fs;
use std::process::Command;
use std::time::Duration;

use common::{Quiet, Reply, agent_with, fake_server, log_messages, scratch, text, tool_call};
use eidra_core::{Agent, AgentOptions, Outcome, Profile};
use serde_json::{Value, json};

fn tool_results(messages: &[Value]) -> Vec<String> {
    let results = messages.iter().filter(|m| m["role"] == "tool");
    results.map(|m| m["content"].as_str().unwrap_or_default().to_owned()).collect()
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
}

#[tokio::test]
async fn changing_a_committed_test_is_pointed_out() {
    if Command::new("git").arg("--version").output().is_err() {
        return eprintln!("no git: skipped");
    }
    let dir = scratch("tests-kept");
    let work = dir.join("work");
    fs::create_dir_all(work.join("tests")).unwrap();
    let test = "from calc import add\n\n\ndef test_adds():\n    assert add(1, 2) == 3\n";
    fs::write(work.join("tests/test_calc.py"), test).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "start"]);
    let url = fake_server(vec![
        // A new test is the agent's own; changing the committed one is pointed out.
        Reply::Chunks(tool_call(
            "edit",
            json!({"path": "tests/test_calc.py", "old": "    assert add(1, 2) == 3\n",
                   "new": "    assert add(1, 2) == 3\n\n\ndef test_more():\n    assert add(2, 2) == 4\n"}),
        )),
        Reply::Chunks(tool_call(
            "edit",
            json!({"path": "tests/test_calc.py", "old": "add(1, 2) == 3", "new": "add(1, 2) == 4"}),
        )),
        Reply::Chunks(text("Done.")),
    ])
    .await;
    let mut agent = agent_with(url, &dir, |_| {});
    agent.push_user("go").unwrap();
    assert_eq!(agent.run(&mut Quiet, 5).await.unwrap(), Outcome::Replied);
    let results = tool_results(&log_messages(&agent));
    assert!(!results[0].contains("already had"), "{}", results[0]);
    assert!(
        results[1].contains("[This changed tests the project already had: test_adds."),
        "{}",
        results[1]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unattended_run_that_changed_committed_tests_hears_so_at_its_first_finish() {
    if Command::new("git").arg("--version").output().is_err() {
        return eprintln!("no git: skipped");
    }
    let dir = scratch("finish-tests");
    let work = dir.join("work");
    fs::create_dir_all(work.join("tests")).unwrap();
    let test = "from calc import add\n\n\ndef test_adds():\n    assert add(1, 2) == 3\n";
    fs::write(work.join("tests/test_calc.py"), test).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "start"]);
    let finish = || Reply::Chunks(tool_call("finish", json!({"summary": "done"})));
    let run = |replies: Vec<Reply>| {
        let (dir, work) = (dir.clone(), work.clone());
        async move {
            let mut profile = Profile::builtin().unwrap();
            profile.base_url = fake_server(replies).await;
            let options = AgentOptions {
                workspace: work,
                session_root: dir.join("sessions"),
                headless: true,
                confirm: vec![],
                model_socket: None,
            };
            let mut agent = Agent::start(profile, options).unwrap();
            agent.push_user("go").unwrap();
            let outcome = agent.run(&mut Quiet, 5).await.unwrap();
            (outcome, tool_results(&log_messages(&agent)))
        }
    };

    // Changed through bash, which the edit's note does not see: the first finish says so, and
    // the next ends the run.
    let sed = json!({"command": "sed -i 's/== 3/== 4/' tests/test_calc.py"});
    let replies = vec![Reply::Chunks(tool_call("bash", sed)), finish(), finish()];
    let (outcome, results) = run(replies).await;
    assert_eq!(outcome, Outcome::Finished("done".into()));
    assert!(
        results[1].starts_with(
            "[Not finished yet. Your changes alter tests the project already had: test_adds in \
             tests/test_calc.py. They say"
        ),
        "{}",
        results[1]
    );
    assert_eq!(results[2], "Finished.");

    // Put back, with a test of its own added, the run finishes at once.
    let add = "printf '\\ndef test_more():\\n    assert True\\n' >> tests/test_calc.py";
    let replies = vec![
        Reply::Chunks(tool_call("bash", json!({"command": "git checkout -q tests/test_calc.py"}))),
        Reply::Chunks(tool_call("bash", json!({"command": add}))),
        finish(),
    ];
    let (outcome, results) = run(replies).await;
    assert_eq!(outcome, Outcome::Finished("done".into()));
    assert_eq!(results[2], "Finished.");
    assert!(fs::read_to_string(work.join("tests/test_calc.py")).unwrap().contains("test_more"));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unattended_run_hears_when_its_budget_is_nearly_spent() {
    let dir = scratch("budget");
    let replies = (0..20).map(|_| Reply::Chunks(tool_call("bash", json!({"command": "true"}))));
    let url = fake_server(replies.collect()).await;
    let mut profile = Profile::builtin().unwrap();
    profile.base_url = url;
    let options = AgentOptions {
        workspace: dir.join("work"),
        session_root: dir.join("sessions"),
        headless: true,
        confirm: vec![],
        model_socket: None,
    };
    let mut agent = Agent::start(profile, options).unwrap();
    agent.push_user("go").unwrap();
    assert_eq!(agent.run(&mut Quiet, 20).await.unwrap(), Outcome::RequestLimit);
    let results = tool_results(&log_messages(&agent));
    let warned: Vec<usize> =
        (0..results.len()).filter(|&i| results[i].contains("left for this task")).collect();
    // Once, with 15% of the 20 requests left.
    assert_eq!(warned, [16]);
    assert!(results[16].ends_with("[Only 3 requests left for this task. Leave the work in a finished, tested state, and call finish before then.]"));

    // A time limit nearly spent says so at the next result.
    let url = fake_server(vec![Reply::Chunks(tool_call("bash", json!({"command": "true"})))]).await;
    let mut profile = Profile::builtin().unwrap();
    profile.base_url = url;
    let options = AgentOptions {
        workspace: dir.join("work"),
        session_root: dir.join("sessions"),
        headless: true,
        confirm: vec![],
        model_socket: None,
    };
    let mut agent = Agent::start(profile, options).unwrap();
    agent.set_time_limit(Duration::from_millis(1));
    agent.push_user("go").unwrap();
    // The run then fails, at a request the server has no reply for.
    let _ = agent.run(&mut Quiet, 10).await;
    let results = tool_results(&log_messages(&agent));
    assert!(results[0].contains("[Only a minute left for this task."), "{}", results[0]);
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn timeout_is_taken_for_timeout_s() {
    let dir = scratch("timeout-alias");
    let url = fake_server(vec![
        Reply::Chunks(tool_call("bash", json!({"command": "sleep 5; echo late", "timeout": 1}))),
        Reply::Chunks(text("Done.")),
    ])
    .await;
    let mut agent = agent_with(url, &dir, |_| {});
    agent.push_user("go").unwrap();
    agent.run(&mut Quiet, 5).await.unwrap();
    let results = tool_results(&log_messages(&agent));
    assert!(results[0].contains("timed out after 1 s"), "{}", results[0]);
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_agent_behind_a_model_socket_is_told_there_is_no_network() {
    let dir = scratch("no-network");
    let options = |socket: Option<&str>| AgentOptions {
        workspace: dir.join("work"),
        session_root: dir.join("sessions"),
        headless: true,
        confirm: vec![],
        model_socket: socket.map(Into::into),
    };
    let system = |agent: &Agent| log_messages(agent)[0]["content"].as_str().unwrap().to_owned();
    let vacuum = Agent::start(Profile::builtin().unwrap(), options(Some("/nonexistent.sock")));
    assert!(system(&vacuum.unwrap()).contains("# Network\nThere is no network here"));
    let open = Agent::start(Profile::builtin().unwrap(), options(None)).unwrap();
    assert!(!system(&open).contains("# Network"));
    let _ = fs::remove_dir_all(&dir);
}
