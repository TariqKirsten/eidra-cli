//! The session log: an append-only JSONL file from which every request is built
//! (docs/DESIGN.md §4.2). Nothing in it is ever rewritten, so the prompt only grows.
//!
//! In an encrypted eidra directory (§18) the log starts with what is needed to find and list it
//! without the key, and every record after that, the header first, is one line of base64: the
//! record sealed with the vault's log key, bound to the session and to its place in the log.

use std::borrow::Cow;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use eidra_llm::{Content, Message, Part, Tool};
use serde::{Deserialize, Serialize};

use crate::event::RequestStats;
use crate::text::utc_datetime;
use crate::vault::{Purpose, Vault};

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
    /// What an encrypted log shows without the key; the header follows, sealed.
    Sealed {
        version: u32,
        id: &'a str,
        created: &'a str,
        workspace: &'a str,
        headless: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<&'a str>,
    },
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
    Sealed {
        id: String,
        created: String,
        workspace: String,
        headless: bool,
        #[serde(default)]
        parent: Option<String>,
    },
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
    /// The log is encrypted.
    pub sealed: bool,
    /// Sealed lines read whole, which the next one written follows.
    sealed_lines: u64,
}

impl Log {
    /// Reads a log; an encrypted one needs the vault it was written with.
    pub fn read(path: &Path, vault: Option<&Vault>) -> Result<Log> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut header = None;
        let mut messages = Vec::new();
        let mut starts = vec![0];
        let lines: Vec<(usize, &str)> =
            text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty()).collect();
        let sealed_as = match lines.first().map(|(_, line)| serde_json::from_str(line)) {
            Some(Ok(Read::Sealed { id, .. })) => Some(id),
            _ => None,
        };
        let mut sealed_lines = 0;
        for (n, &(i, line)) in lines.iter().enumerate() {
            // A line cut short by a crash, at the very end, is left out.
            let cut_short = n + 1 == lines.len() && !text.ends_with('\n');
            let json = match &sealed_as {
                Some(_) if n == 0 => continue,
                Some(id) => {
                    let Some(vault) = vault else {
                        bail!("session {id} is encrypted, and ~/.eidra is not unlocked");
                    };
                    match open_line(vault, id, n as u64 - 1, line) {
                        Ok(json) => Cow::Owned(json),
                        Err(_) if cut_short => break,
                        Err(e) => {
                            return Err(e)
                                .with_context(|| format!("{} line {}", path.display(), i + 1));
                        }
                    }
                }
                None => Cow::Borrowed(line),
            };
            let record = match serde_json::from_str(&json) {
                Ok(record) => record,
                Err(_) if cut_short => break,
                Err(e) => {
                    return Err(e).with_context(|| format!("{} line {}", path.display(), i + 1));
                }
            };
            if sealed_as.is_some() {
                sealed_lines += 1;
            }
            match record {
                Read::Session(h) => header = Some(h),
                Read::Message { message, stats } => messages.push((message, stats)),
                Read::Sealed { .. } | Read::End {} | Read::Resumed {} => {}
                Read::Compaction {} => starts.push(messages.len()),
            }
        }
        let Some(header) = header else {
            bail!("{} has no session header", path.display());
        };
        Ok(Log { header, messages, starts, sealed: sealed_as.is_some(), sealed_lines })
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
    /// Locked while the session is open, so that no other eidra goes on with it or erases it.
    file: File,
    messages: Vec<Message>,
    sealing: Option<Sealing>,
}

/// How an encrypted log's next line is sealed.
struct Sealing {
    vault: Arc<Vault>,
    /// The line's place among the sealed ones, which it is bound to.
    next: u64,
}

impl Session {
    /// Starts a log in `root`, as `<id>.jsonl` beside a directory `<id>/` for saved output; an
    /// encrypted one with a vault. `header.id` and `header.created` are filled in here.
    pub fn create(root: &Path, mut header: Header, vault: Option<Arc<Vault>>) -> Result<Self> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let (y, mo, d, h, mi, s) = utc_datetime(now.as_secs());
        let salt = (now.subsec_nanos() ^ std::process::id().rotate_left(16)) & 0xffff;
        header.id = format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{salt:04x}");
        header.created = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");

        let dir = root.join(&header.id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = root.join(format!("{}.jsonl", header.id));
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        lock(&file, &header.id)?;
        if vault.is_some() {
            let envelope = Record::Sealed {
                version: 1,
                id: &header.id,
                created: &header.created,
                workspace: &header.workspace,
                headless: header.headless,
                parent: header.parent.as_deref(),
            };
            let mut line = serde_json::to_vec(&envelope)?;
            line.push(b'\n');
            file.write_all(&line).with_context(|| format!("writing {}", path.display()))?;
        }
        let sealing = vault.map(|vault| Sealing { vault, next: 0 });
        let mut session = Session { header, path, dir, file, messages: Vec::new(), sealing };
        session.write(&Record::Session(&session.header.clone()))?;
        Ok(session)
    }

    /// Opens a log to go on with it: the session's messages are its last conversation's, and
    /// what follows is appended. The log as read comes back too. With a vault, only an encrypted
    /// log goes on: what is added to a plain one would not be encrypted either.
    pub fn open(path: &Path, vault: Option<Arc<Vault>>) -> Result<(Self, Log)> {
        let log = Log::read(path, vault.as_deref())?;
        if vault.is_some() && !log.sealed {
            bail!(
                "session {} is not encrypted, so it does not go on in an encrypted ~/.eidra: \
                 /erase-unsecure erases it, or EIDRA_VAULT=off goes on with it unencrypted",
                log.header.id
            );
        }
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        lock(&file, &log.header.id)?;
        // A line cut short by a crash goes, so that what follows starts on a line of its own and
        // the log reads back whole.
        let mut text = Vec::new();
        file.read_to_end(&mut text)?;
        if !text.is_empty() && !text.ends_with(b"\n") {
            let end = text.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            file.set_len(end as u64)?;
            file.seek(SeekFrom::End(0))?;
        }
        let dir = path.with_file_name(&log.header.id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let start = log.starts.last().copied().unwrap_or(0);
        let messages = log.messages[start..].iter().map(|(m, _)| m.clone()).collect();
        let sealing = vault.map(|vault| Sealing { vault, next: log.sealed_lines });
        let header = log.header.clone();
        let session = Session { header, path: path.to_path_buf(), dir, file, messages, sealing };
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
        if let Some(sealing) = &mut self.sealing {
            let aad = line_aad(&self.header.id, sealing.next);
            let sealed = sealing.vault.seal(Purpose::Log, aad.as_bytes(), &line);
            sealing.next += 1;
            line = STANDARD.encode(sealed).into_bytes();
        }
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

/// Whether another eidra has the session logged at `path` open.
pub fn in_use(path: &Path) -> bool {
    File::open(path).is_ok_and(|file| matches!(file.try_lock(), Err(TryLockError::WouldBlock)))
}

/// What a sealed line is bound to: its session and its place.
fn line_aad(id: &str, n: u64) -> String {
    format!("{id}:{n}")
}

/// A sealed line's record, as JSON.
fn open_line(vault: &Vault, id: &str, n: u64, line: &str) -> Result<String> {
    let sealed = STANDARD.decode(line.trim()).context("an encrypted line that is not base64")?;
    let json = vault.open(Purpose::Log, line_aad(id, n).as_bytes(), &sealed)?;
    String::from_utf8(json).context("an encrypted line that is not UTF-8")
}

/// A session as `eidra sessions` lists it, from the start of its log.
#[derive(Debug, Clone)]
pub struct Summary {
    pub path: PathBuf,
    /// Without the key, an encrypted log's header has only its id, time, workspace, whether it
    /// ran unattended, and its parent.
    pub header: Header,
    /// When the log was last written to.
    pub modified: SystemTime,
    /// The first thing the user said, if it could be read.
    pub first: Option<String>,
    /// The log is encrypted.
    pub sealed: bool,
}

/// The sessions in `root`, but not subagents', the most recently active first. Only the start of
/// each log is read; an encrypted one's first message only with the vault.
pub fn list(root: &Path, vault: Option<&Vault>) -> Vec<Summary> {
    let mut sessions: Vec<Summary> =
        all(root, vault).into_iter().filter(|summary| summary.header.parent.is_none()).collect();
    sessions.sort_by_key(|summary| std::cmp::Reverse(summary.modified));
    sessions
}

/// Every log in `root`, subagents' too, in no order.
pub fn all(root: &Path, vault: Option<&Vault>) -> Vec<Summary> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|path| summary(&path, vault))
        .collect()
}

fn summary(path: &Path, vault: Option<&Vault>) -> Option<Summary> {
    let modified = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let mut lines = BufReader::new(File::open(path).ok()?).lines();
    let (header, sealed_as, mut lines): (Header, Option<String>, Vec<String>) =
        match serde_json::from_str(&lines.next()?.ok()?).ok()? {
            Read::Session(header) => (header, None, lines.take(4).map_while(Result::ok).collect()),
            Read::Sealed { id, created, workspace, headless, parent } => {
                // The system prompt comes first, then the user; a few lines are enough.
                let rest: Vec<String> = lines.take(5).map_while(Result::ok).collect();
                let opened: Vec<String> = match vault {
                    Some(vault) => rest
                        .iter()
                        .enumerate()
                        .map_while(|(n, line)| open_line(vault, &id, n as u64, line).ok())
                        .collect(),
                    None => Vec::new(),
                };
                let full = opened.first().and_then(|json| match serde_json::from_str(json) {
                    Ok(Read::Session(header)) => Some(header),
                    _ => None,
                });
                let header = full.unwrap_or_else(|| Header {
                    version: 1,
                    id: id.clone(),
                    created,
                    workspace,
                    headless,
                    profile: String::new(),
                    profile_hash: String::new(),
                    base_url: String::new(),
                    model: String::new(),
                    reasoning_effort: String::new(),
                    tools: Vec::new(),
                    parent,
                    task: None,
                });
                (header, Some(id), opened.into_iter().skip(1).collect())
            }
            _ => return None,
        };
    let first = lines.drain(..).find_map(|line| match serde_json::from_str(&line).ok()? {
        Read::Message { message: Message::User { content }, .. } => Some(text_of(&content)),
        _ => None,
    });
    Some(Summary { path: path.to_path_buf(), header, modified, first, sealed: sealed_as.is_some() })
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
        let mut session = Session::create(&root, header, None).unwrap();
        session.push(Message::user("hi"), None).unwrap();
        session.end("finished", "done").unwrap();

        let text = fs::read_to_string(session.path()).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        let log = Log::read(session.path(), None).unwrap();
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
        let mut session = Session::create(&root, header, None).unwrap();
        session.push(Message::system("s"), None).unwrap();
        session.push(Message::user("long task"), None).unwrap();
        let fresh = vec![Message::system("s"), Message::user("summary")];
        session.compact(200_000, "summary", None, fresh.clone()).unwrap();
        assert_eq!(session.messages(), fresh.as_slice());
        let log = Log::read(session.path(), None).unwrap();
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
        let mut session = Session::create(&root, header, None).unwrap();
        session.push(Message::system("s"), None).unwrap();
        session.push(Message::user("first"), None).unwrap();
        let path = session.path().to_path_buf();
        drop(session);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"type":"message","message":{"role":"assis"#).unwrap();
        drop(file);

        assert_eq!(Log::read(&path, None).unwrap().messages.len(), 2);
        let (mut session, _) = Session::open(&path, None).unwrap();
        assert_eq!(session.messages().len(), 2);
        session.resumed("h").unwrap();
        session.push(Message::user("again"), None).unwrap();
        let log = Log::read(&path, None).unwrap();
        assert_eq!(log.messages.len(), 3);
        assert!(!fs::read_to_string(&path).unwrap().contains("assis"));
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
    fn an_encrypted_log_reads_back_only_with_its_vault_and_goes_on_from_a_crash() {
        let root = std::env::temp_dir().join(format!("eidra-sealed-test-{}", std::process::id()));
        let vault = Arc::new(Vault::with_key([3; 32]));
        let mut session =
            Session::create(&root, header("/secret/work", false), Some(vault.clone())).unwrap();
        session.push(Message::system("the system prompt"), None).unwrap();
        session.push(Message::user("the password is swordfish"), None).unwrap();
        let path = session.path().to_path_buf();
        // Another eidra cannot go on with it while it is open.
        assert!(in_use(&path));
        let busy = Session::open(&path, Some(vault.clone())).err().unwrap();
        assert!(busy.to_string().contains("open in another eidra"), "{busy}");
        drop(session);
        assert!(!in_use(&path));

        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("swordfish") && !text.contains("system prompt"), "{text}");
        assert!(text.lines().next().unwrap().contains("/secret/work"), "the envelope says where");
        let locked = Log::read(&path, None).err().unwrap();
        assert!(locked.to_string().contains("is encrypted"), "{locked}");
        let other = Vault::with_key([4; 32]);
        assert!(Log::read(&path, Some(&other)).is_err());
        let log = Log::read(&path, Some(&vault)).unwrap();
        assert!(log.sealed && log.header.workspace == "/secret/work");
        assert_eq!(log.messages.len(), 2);

        // A crash cut the last line short: it goes, and the session goes on after it.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"c2VhbGVkIGJ1dCBjdXQ").unwrap();
        drop(file);
        assert_eq!(Log::read(&path, Some(&vault)).unwrap().messages.len(), 2);
        let (mut session, _) = Session::open(&path, Some(vault.clone())).unwrap();
        session.push(Message::user("again"), None).unwrap();
        drop(session);
        let log = Log::read(&path, Some(&vault)).unwrap();
        assert_eq!(log.messages.last().unwrap().0, Message::user("again"));

        // Listed without the key: where and when, but not what was said.
        let listed = &list(&root, None)[0];
        assert!(listed.sealed && listed.first.is_none());
        assert_eq!(listed.header.workspace, "/secret/work");
        let listed = &list(&root, Some(&vault))[0];
        assert_eq!(listed.first.as_deref(), Some("the password is swordfish"));
        assert_eq!(listed.header.model, "m");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_plain_log_does_not_go_on_in_an_encrypted_directory() {
        let root = std::env::temp_dir().join(format!("eidra-plain-test-{}", std::process::id()));
        let mut session = Session::create(&root, header("/work", false), None).unwrap();
        session.push(Message::user("hi"), None).unwrap();
        let path = session.path().to_path_buf();
        drop(session);
        let vault = Arc::new(Vault::with_key([3; 32]));
        // It can be read, but nothing more is written to it.
        assert!(Log::read(&path, Some(&vault)).is_ok());
        let refused = Session::open(&path, Some(vault)).err().unwrap();
        assert!(refused.to_string().contains("is not encrypted"), "{refused}");
        assert!(!list(&root, None)[0].sealed);
        fs::remove_dir_all(root).unwrap();
    }
}
