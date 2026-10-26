//! The session log: an append-only JSONL file from which every request is built
//! (docs/DESIGN.md §4.2). Nothing in it is ever rewritten, so the prompt only grows.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use eidra_llm::{Message, Tool};
use serde::Serialize;

use crate::event::RequestStats;
use crate::text::utc_datetime;

/// What a session started with: the log's first line.
#[derive(Debug, Clone, Serialize)]
pub struct Header {
    pub version: u32,
    pub id: String,
    pub created: String,
    pub workspace: String,
    pub headless: bool,
    pub profile: String,
    pub profile_hash: String,
    pub base_url: String,
    pub model: String,
    pub reasoning_effort: String,
    pub tools: Vec<Tool>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record<'a> {
    Session(&'a Header),
    Message {
        message: &'a Message,
        #[serde(skip_serializing_if = "Option::is_none")]
        stats: Option<&'a RequestStats>,
    },
    End {
        outcome: &'a str,
        detail: &'a str,
    },
}

pub struct Session {
    header: Header,
    path: PathBuf,
    dir: PathBuf,
    file: File,
    messages: Vec<Message>,
}

impl Session {
    /// Starts a log in `root`, as `<id>.jsonl` beside a directory `<id>/` for saved output.
    /// `header.id` and `header.created` are filled in here.
    pub fn create(root: &Path, mut header: Header) -> Result<Self> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let (y, mo, d, h, mi, s) = utc_datetime(now.as_secs());
        let salt = (now.subsec_nanos() ^ std::process::id().rotate_left(16)) & 0xffff;
        header.id = format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{salt:04x}");
        header.created = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");

        let dir = root.join(&header.id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = root.join(format!("{}.jsonl", header.id));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        let mut session = Session { header, path, dir, file, messages: Vec::new() };
        session.write(&Record::Session(&session.header.clone()))?;
        Ok(session)
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The session's directory, for saved tool output and shell state.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn push(&mut self, message: Message, stats: Option<&RequestStats>) -> Result<()> {
        self.write(&Record::Message { message: &message, stats })?;
        self.messages.push(message);
        Ok(())
    }

    pub fn end(&mut self, outcome: &str, detail: &str) -> Result<()> {
        self.write(&Record::End { outcome, detail })
    }

    fn write(&mut self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.file.write_all(&line).with_context(|| format!("writing {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn writes_a_header_then_one_line_per_message() {
        let root = std::env::temp_dir().join(format!("eidra-session-test-{}", std::process::id()));
        let header = Header {
            version: 1,
            id: String::new(),
            created: String::new(),
            workspace: "/work".into(),
            headless: true,
            profile: "p".into(),
            profile_hash: "h".into(),
            base_url: "http://x/v1".into(),
            model: "m".into(),
            reasoning_effort: "medium".into(),
            tools: vec![],
        };
        let mut session = Session::create(&root, header).unwrap();
        session.push(Message::user("hi"), None).unwrap();
        session.end("finished", "done").unwrap();

        let text = fs::read_to_string(session.path()).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["id"], session.header().id.as_str());
        assert_eq!(lines[1]["message"]["content"], "hi");
        assert_eq!(lines[2]["outcome"], "finished");
        assert!(session.dir().is_dir());
        fs::remove_dir_all(root).unwrap();
    }
}
