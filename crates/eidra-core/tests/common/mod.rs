//! A fake server that streams scripted replies, and what the tests around it share.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eidra_core::{Agent, AgentOptions, Approval, Decision, Event, Frontend, Profile};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How the fake server answers one request.
pub enum Reply {
    /// These chunks, then the end of the stream.
    Chunks(Vec<Value>),
    /// These chunks, then nothing for a long time.
    Stall(Vec<Value>),
    /// This status, with this JSON body, as a server refusing the request.
    Refuse(u16, Value),
}

/// Serves `replies` in order, one per request, and returns the API root.
pub async fn fake_server(replies: Vec<Reply>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        for reply in replies {
            let (mut conn, _) = listener.accept().await.unwrap();
            read_request(&mut conn).await;
            let (chunks, stall) = match reply {
                Reply::Chunks(chunks) => (chunks, false),
                Reply::Stall(chunks) => (chunks, true),
                Reply::Refuse(status, body) => {
                    let body = body.to_string();
                    let head = format!(
                        "HTTP/1.1 {status} Refused\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    conn.write_all(head.as_bytes()).await.unwrap();
                    conn.write_all(body.as_bytes()).await.unwrap();
                    continue;
                }
            };
            let mut body = String::new();
            for chunk in chunks {
                body.push_str(&format!("data: {chunk}\n\n"));
            }
            if !stall {
                body.push_str("data: [DONE]\n\n");
            }
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            conn.write_all(head.as_bytes()).await.unwrap();
            conn.write_all(body.as_bytes()).await.unwrap();
            if stall {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    });
    url
}

/// Reads one request's head and body, which the fake server does not look at.
async fn read_request(conn: &mut tokio::net::TcpStream) {
    let mut seen = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = conn.read(&mut buf).await.unwrap();
        seen.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&seen);
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text[..end]
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())?
                })
                .unwrap_or(0usize);
            if seen.len() >= end + 4 + length {
                return;
            }
        }
        if n == 0 {
            return;
        }
    }
}

pub fn tool_call(name: &str, arguments: Value) -> Vec<Value> {
    tool_calls(&[(name, arguments)])
}

/// One reply calling several tools.
pub fn tool_calls(calls: &[(&str, Value)]) -> Vec<Value> {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, arguments))| {
            json!({"index": i, "id": format!("call_{}", i + 1), "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}})
        })
        .collect();
    vec![
        json!({"choices": [{"index": 0, "delta": {"tool_calls": calls}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ]
}

/// A reply of text, calling no tool.
pub fn text(content: &str) -> Vec<Value> {
    vec![
        json!({"choices": [{"index": 0, "delta": {"content": content}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ]
}

/// A frontend that keeps nothing and declines everything.
pub struct Quiet;

impl Frontend for Quiet {
    fn event(&mut self, _event: Event) {}

    async fn approve(&mut self, _approval: &Approval) -> Decision {
        Decision::Deny { note: None }
    }
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("eidra-interrupt-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("work")).unwrap();
    dir
}

/// An interactive agent with the built-in profile, working in `dir`/work.
pub fn agent(base_url: String, dir: &Path) -> Agent {
    agent_with(base_url, dir, |_| {})
}

pub fn agent_with(base_url: String, dir: &Path, change: impl FnOnce(&mut Profile)) -> Agent {
    let mut profile = Profile::builtin().unwrap();
    profile.base_url = base_url;
    change(&mut profile);
    let options = AgentOptions {
        workspace: dir.join("work"),
        session_root: dir.join("sessions"),
        headless: false,
        confirm: vec![],
        model_socket: None,
    };
    Agent::start(profile, options).unwrap()
}

pub fn log_messages(agent: &Agent) -> Vec<Value> {
    fs::read_to_string(agent.session_log())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|record| record["type"] == "message")
        .map(|record| record["message"].clone())
        .collect()
}
