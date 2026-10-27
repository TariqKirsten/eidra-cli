//! The session log: an append-only JSONL file from which every request is built
//! (docs/DESIGN.md §4.2). Nothing in it is ever rewritten, so the prompt only grows.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use eidra_llm::{Content, Message, Part, Tool};
use serde::{Deserialize, Serialize};

use crate::event::RequestStats;
use crate::text::utc_datetime;

/// What a session started with: the log's first line.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// For a subagent: the session that started it, and the task's description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
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
    /// The conversation grew past the profile's threshold and goes on in a new one, from a summary
    /// (docs/DESIGN.md §8). The new conversation's messages follow.
    Compaction {
        tokens: u64,
        summary: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        stats: Option<&'a RequestStats>,
    },
    /// The session was taken up again, at this time, with this profile (docs/DESIGN.md §14).
    Resumed {
        at: &'a str,
        profile_hash: &'a str,
    },
}

/// A log's records, read back.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Read {
    Session(Header),
    Message {
        message: Message,
        #[serde(default)]
        stats: Option<RequestStats>,
    },
    End {},
    Compaction {},
    Resumed {},
}

/// A session log read back, for replaying its requests.
pub struct Log {
    pub header: Header,
    /// Every message in order, each reply with the stats of the request that produced it.
    pub messages: Vec<(Message, Option<RequestStats>)>,
    /// Where each conversation begins in `messages`: 0, and after each compaction.
    pub starts: Vec<usize>,
}

impl Log {
    pub fn read(path: &Path) -> Result<Log> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Log::parse(&text, path)
    }

    /// A log's text read back; `path` names it in errors. A last line without its newline was cut
    /// short by a crash, however whole it looks, and is left out: a session that goes on cuts it
    /// off the file.
    fn parse(text: &str, path: &Path) -> Result<Log> {
        let mut header = None;
        let mut messages = Vec::new();
        let mut starts = vec![0];
        let lines: Vec<(usize, &str)> =
            text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty()).collect();
        for (n, &(i, line)) in lines.iter().enumerate() {
            if n + 1 == lines.len() && !text.ends_with('\n') {
                break;
            }
            let record = serde_json::from_str(line)
                .with_context(|| format!("{} line {}", path.display(), i + 1))?;
            match record {
                Read::Session(h) => header = Some(h),
                Read::Message { message, stats } => messages.push((message, stats)),
                Read::End {} | Read::Resumed {} => {}
                Read::Compaction {} => starts.push(messages.len()),
            }
        }
        let Some(header) = header else {
            bail!("{} has no session header", path.display());
        };
        Ok(Log { header, messages, starts })
    }

    /// Where the conversation that holds message `index` begins.
    pub fn start_of(&self, index: usize) -> usize {
        self.starts.iter().copied().filter(|&start| start <= index).max().unwrap_or(0)
    }
}

pub struct Session {
    header: Header,
    path: PathBuf,
    dir: PathBuf,
    /// Locked while the session is open, so that no other eidra goes on with it.
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
        lock(&file, &header.id)?;
        let mut session = Session { header, path, dir, file, messages: Vec::new() };
        session.write(&Record::Session(&session.header.clone()))?;
        Ok(session)
    }

    /// Opens a log to go on with it: the session's messages are its last conversation's, and
    /// what follows is appended. The log as read comes back too.
    pub fn open(path: &Path) -> Result<(Self, Log)> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // Locked before it is read, so that no other eidra adds to it in between.
        let id = path.file_stem().unwrap_or_default().to_string_lossy();
        lock(&file, &id)?;
        let mut text = Vec::new();
        file.read_to_end(&mut text).with_context(|| format!("reading {}", path.display()))?;
        let text = String::from_utf8(text)
            .with_context(|| format!("reading {}: it is not UTF-8", path.display()))?;
        let log = Log::parse(&text, path)?;
        // A line cut short by a crash goes, so that what follows starts on a line of its own and
        // the log reads back whole.
        if !text.is_empty() && !text.ends_with('\n') {
            let end = text.rfind('\n').map_or(0, |i| i + 1);
            file.set_len(end as u64)?;
            file.seek(SeekFrom::End(0))?;
        }
        let dir = path.with_file_name(&log.header.id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let start = log.starts.last().copied().unwrap_or(0);
        let messages = log.messages[start..].iter().map(|(m, _)| m.clone()).collect();
        let session =
            Session { header: log.header.clone(), path: path.to_path_buf(), dir, file, messages };
        Ok((session, log))
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Marks where the session was taken up again, with the profile it goes on with.
    pub fn resumed(&mut self, profile_hash: &str) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let (y, mo, d, h, mi, s) = utc_datetime(now.as_secs());
        let at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
        self.write(&Record::Resumed { at: &at, profile_hash })
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

    /// Ends the conversation at `tokens` and starts a new one of `messages`, from `summary`.
    pub fn compact(
        &mut self,
        tokens: u64,
        summary: &str,
        stats: Option<&RequestStats>,
        messages: Vec<Message>,
    ) -> Result<()> {
        self.write(&Record::Compaction { tokens, summary, stats })?;
        self.messages.clear();
        for message in messages {
            self.push(message, None)?;
        }
        Ok(())
    }

    fn write(&mut self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.file.write_all(&line).with_context(|| format!("writing {}", self.path.display()))
    }
}

impl Drop for Session {
    /// Lets go of the lock at once: a command started a moment ago may hold a copy of the file
    /// until it runs, and the lock lasts while any copy is open.
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Takes the lock that marks a session open.
fn lock(file: &File, id: &str) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => bail!("session {id} is open in another eidra"),
        // A filesystem without locks: the session goes on unlocked.
        Err(TryLockError::Error(_)) => Ok(()),
    }
}

/// A session as `eidra sessions` lists it, from the start of its log.
#[derive(Debug, Clone)]
pub struct Summary {
    pub path: PathBuf,
    pub header: Header,
    /// When the log was last written to.
    pub modified: SystemTime,
    /// The first thing the user said.
    pub first: Option<String>,
}

/// The sessions in `root`, but not subagents', the most recently active first. Only the start of
/// each log is read.
pub fn list(root: &Path) -> Vec<Summary> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sessions: Vec<Summary> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|path| summary(&path))
        .filter(|summary| summary.header.parent.is_none())
        .collect();
    sessions.sort_by_key(|summary| std::cmp::Reverse(summary.modified));
    sessions
}

fn summary(path: &Path) -> Option<Summary> {
    let modified = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let mut lines = BufReader::new(File::open(path).ok()?).lines();
    let Ok(Read::Session(header)) = serde_json::from_str(&lines.next()?.ok()?) else {
        return None;
    };
    // The system prompt comes first, then the user; a few lines are enough.
    let first = lines.take(4).map_while(Result::ok).find_map(|line| {
        match serde_json::from_str(&line).ok()? {
            Read::Message { message: Message::User { content }, .. } => Some(text_of(&content)),
            _ => None,
        }
    });
    Some(Summary { path: path.to_path_buf(), header, modified, first })
}

/// The text of a message's content, without its images.
pub fn text_of(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
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
            parent: None,
            task: None,
        };
        let mut session = Session::create(&root, header).unwrap();
        session.push(Message::user("hi"), None).unwrap();
        session.end("finished", "done").unwrap();

        let text = fs::read_to_string(session.path()).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        let log = Log::read(session.path()).unwrap();
        assert_eq!(log.header.id, session.header().id);
        assert_eq!(log.messages, vec![(Message::user("hi"), None)]);
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["id"], session.header().id.as_str());
        assert_eq!(lines[1]["message"]["content"], "hi");
        assert_eq!(lines[2]["outcome"], "finished");
        assert!(session.dir().is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_compaction_starts_a_new_conversation_in_the_same_log() {
        let root = std::env::temp_dir().join(format!("eidra-compact-test-{}", std::process::id()));
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
            parent: None,
            task: None,
        };
        let mut session = Session::create(&root, header).unwrap();
        session.push(Message::system("s"), None).unwrap();
        session.push(Message::user("long task"), None).unwrap();
        let fresh = vec![Message::system("s"), Message::user("summary")];
        session.compact(200_000, "summary", None, fresh.clone()).unwrap();
        assert_eq!(session.messages(), fresh.as_slice());
        let log = Log::read(session.path()).unwrap();
        assert_eq!(log.messages.len(), 4);
        assert_eq!(log.starts, [0, 2]);
        assert_eq!((log.start_of(1), log.start_of(3)), (0, 2));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_line_cut_short_by_a_crash_goes_when_the_session_goes_on() {
        let root = std::env::temp_dir().join(format!("eidra-crash-test-{}", std::process::id()));
        let header = Header {
            version: 1,
            id: String::new(),
            created: String::new(),
            workspace: "/work".into(),
            headless: false,
            profile: "p".into(),
            profile_hash: "h".into(),
            base_url: "http://x/v1".into(),
            model: "m".into(),
            reasoning_effort: "medium".into(),
            tools: vec![],
            parent: None,
            task: None,
        };
        let mut session = Session::create(&root, header).unwrap();
        session.push(Message::system("s"), None).unwrap();
        session.push(Message::user("first"), None).unwrap();
        let path = session.path().to_path_buf();
        drop(session);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"type":"message","message":{"role":"assis"#).unwrap();
        drop(file);

        assert_eq!(Log::read(&path).unwrap().messages.len(), 2);
        let (mut session, _) = Session::open(&path).unwrap();
        assert_eq!(session.messages().len(), 2);
        session.resumed("h").unwrap();
        session.push(Message::user("again"), None).unwrap();
        let log = Log::read(&path).unwrap();
        assert_eq!(log.messages.len(), 3);
        assert!(!fs::read_to_string(&path).unwrap().contains("assis"));
        drop(session);

        // A last line written whole but for its newline was cut short too, however whole it
        // looks: it goes, and the line written after it takes its place.
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.strip_suffix('\n').unwrap()).unwrap();
        assert_eq!(Log::read(&path).unwrap().messages.len(), 2);
        let (mut session, log) = Session::open(&path).unwrap();
        assert_eq!((log.messages.len(), session.messages().len()), (2, 2));
        session.push(Message::user("once more"), None).unwrap();
        let log = Log::read(&path).unwrap();
        assert_eq!(log.messages.len(), 3);
        assert_eq!(log.messages[2].0, Message::user("once more"));
        fs::remove_dir_all(root).unwrap();
    }

    fn header(workspace: &str, headless: bool) -> Header {
        Header {
            version: 1,
            id: String::new(),
            created: String::new(),
            workspace: workspace.into(),
            headless,
            profile: "p".into(),
            profile_hash: "h".into(),
            base_url: "http://x/v1".into(),
            model: "m".into(),
            reasoning_effort: "medium".into(),
            tools: vec![],
            parent: None,
            task: None,
        }
    }

    #[test]
    fn a_session_is_open_in_one_eidra_at_a_time() {
        let root = std::env::temp_dir().join(format!("eidra-lock-test-{}", std::process::id()));
        let mut session = Session::create(&root, header("/work", false)).unwrap();
        session.push(Message::user("hi"), None).unwrap();
        let path = session.path().to_path_buf();
        let busy = Session::open(&path).err().unwrap();
        assert!(busy.to_string().contains("open in another eidra"), "{busy}");
        drop(session);
        // Once it is left, another goes on with it, and has it to itself in turn.
        let (again, _) = Session::open(&path).unwrap();
        assert!(Session::open(&path).is_err());
        drop(again);
        assert!(Session::open(&path).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
