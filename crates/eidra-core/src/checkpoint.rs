//! Checkpoints (docs/DESIGN.md §14): a git repository of eidra's own records the workspace before
//! and after each turn of an interactive session, so that `/undo` can put back what a turn
//! changed and `/diff` can show it. It sees what commands changed as well as what the file tools
//! did, and never touches the project's own repository, index or branches.
//!
//! The repository is bare, one per workspace, under eidra's data directory. Each snapshot is a
//! commit on a ref of the session's own, `refs/eidra/<session>`, whose subjects say which turn it
//! belongs to, so a resumed session finds its turns again. The project's ignore files apply, and
//! a few defaults for caches and environments; a new file over 8 MiB is left out for good.
//!
//! In an encrypted eidra directory (§18) git sends every file through eidra's filter, which
//! seals what goes into a snapshot and opens what comes out, and each turn's label goes into the
//! commit's message sealed. The names of the files stay readable.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use zeroize::Zeroizing;

use crate::snapshot_filter::{KEY_ENV, key_hex};
use crate::text::fnv1a;
use crate::vault::{Purpose, Vault};

/// The longest one git command may take. Past it, checkpoints are off for the session.
const TIMEOUT: Duration = Duration::from_secs(20);
/// A new file larger than this is left out of every snapshot.
const LARGEST_FILE: u64 = 8 << 20;
/// New files one snapshot takes at most: more means a workspace too big to snapshot each turn.
const MOST_NEW_FILES: usize = 20_000;
/// A session's index is removed this long after it was last used.
const INDEX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// A session's snapshots are let go this long after its last one, and git then prunes them.
const SESSION_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// Left out on top of the project's own ignore files: caches, environments and build output,
/// which no undo needs.
const EXCLUDE: &str = "\
.venv/
venv/
node_modules/
__pycache__/
*.pyc
.mypy_cache/
.ruff_cache/
.pytest_cache/
.tox/
/target/
";
/// Every file goes through eidra's filter, and is compared through it, and is kept as its exact
/// bytes: the repository's own attributes come before the project's, so none of the project's
/// filters, diff drivers, line-ending or encoding conversions apply. Git would make those to the
/// sealed bytes, which would then no longer open, and /undo would lose the file. With no filter
/// set up, in an eidra directory that is not encrypted, only the exact bytes remain.
const ATTRIBUTES: &str = "* filter=eidra diff=eidra -text -ident -working-tree-encoding\n";
/// Left in a repository whose pruning an erase put off, while an open session used it.
const PRUNE_PENDING: &str = "eidra-prune-pending";
/// What a sealed label starts with in a snapshot's message.
const LABEL: &str = "label ";
const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "eidra"),
    ("GIT_AUTHOR_EMAIL", "eidra@localhost"),
    ("GIT_COMMITTER_NAME", "eidra"),
    ("GIT_COMMITTER_EMAIL", "eidra@localhost"),
];

/// One turn's snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub number: usize,
    /// The start of the message that began it.
    pub label: String,
    pub before: String,
    /// None while the turn runs.
    pub after: Option<String>,
}

/// What an undo did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Undone {
    pub turn: usize,
    pub label: String,
    /// Files put back as they were before the turn.
    pub restored: Vec<String>,
    /// Files the turn made, removed.
    pub removed: Vec<String>,
    /// Files the turn changed that have changed again since, left as they are.
    pub kept: Vec<String>,
}

pub struct Checkpoints {
    /// Where the repositories of all workspaces are.
    root: PathBuf,
    git_dir: PathBuf,
    workspace: PathBuf,
    /// The session's own index, so that two sessions in one workspace do not share one.
    index: PathBuf,
    reference: String,
    /// The session's turns, oldest first, without those undone.
    turns: Vec<Turn>,
    /// The latest snapshot, which the next one follows.
    last: Option<String>,
    /// Why checkpoints stopped, once a snapshot failed.
    off: Option<String>,
    /// The number the next turn gets: past every turn's, undone ones too.
    next: usize,
    /// The repository has been made, if it had to be.
    ready: bool,
    sealed: Option<Sealed>,
}

/// What the snapshots of an encrypted eidra directory need.
struct Sealed {
    /// The eidra binary, which git runs as the filter.
    program: PathBuf,
    /// The snapshot key, as the filter gets it.
    key: Zeroizing<String>,
    /// For the turns' labels.
    vault: Arc<Vault>,
}

impl Checkpoints {
    /// The checkpoints of a new session in `workspace`, kept in a repository under `root` that
    /// the first snapshot makes if need be.
    pub fn new(root: &Path, workspace: &Path, session: &str) -> Result<Self, String> {
        let workspace = workspace.canonicalize().map_err(|e| format!("the workspace: {e}"))?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if workspace == Path::new("/") || home.is_some_and(|home| home == workspace) {
            return Err("they are off in the home and root directories, too big to snapshot".into());
        }
        let git_dir = root.join(repository_name(&workspace));
        Ok(Checkpoints {
            root: root.to_path_buf(),
            index: git_dir.join(format!("eidra-index-{session}")),
            git_dir,
            workspace,
            reference: format!("refs/eidra/{session}"),
            turns: Vec::new(),
            last: None,
            off: None,
            next: 1,
            ready: false,
            sealed: None,
        })
    }

    /// Seals the snapshots with `vault`'s key, through `program`, the eidra binary, which git
    /// runs as its filter (`eidra snapshot-filter`).
    pub fn sealed(mut self, program: &Path, vault: Arc<Vault>) -> Self {
        let key = key_hex(&vault.snapshot_key());
        self.sealed = Some(Sealed { program: program.to_path_buf(), key, vault });
        self
    }

    /// The checkpoints of a session that may have turns already, read from its ref.
    pub async fn resume(root: &Path, workspace: &Path, session: &str) -> Result<Self, String> {
        Self::new(root, workspace, session)?.resumed().await
    }

    /// These checkpoints, with the turns a session that goes on already had.
    pub async fn resumed(mut self) -> Result<Self, String> {
        if self.git_dir.join("HEAD").exists() {
            self.load().await?;
        }
        Ok(self)
    }

    /// Makes the repository if there is none yet.
    async fn ready(&mut self) -> Result<(), String> {
        if self.ready {
            return Ok(());
        }
        if !self.git_dir.join("HEAD").exists() {
            self.create().await?;
        } else {
            self.remove_old_sessions().await;
        }
        self.remove_old_indexes();
        // Repositories from before the attributes get them too.
        let attributes = self.git_dir.join("info/attributes");
        if fs::read_to_string(&attributes).ok().as_deref() != Some(ATTRIBUTES) {
            fs::write(&attributes, ATTRIBUTES).map_err(|e| e.to_string())?;
        }
        self.ready = true;
        Ok(())
    }

    /// Lets go of the refs of sessions whose last snapshot is older than `SESSION_AGE`, and asks
    /// git to collect what they held. `gc --auto` works only past its own thresholds, and in the
    /// background. Nothing here stops the session if it fails.
    async fn remove_old_sessions(&self) {
        let format = "--format=%(refname) %(committerdate:unix)";
        let Ok(refs) = self.git(&["for-each-ref", format, "refs/eidra/"]).await else {
            return;
        };
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        for line in refs.lines() {
            let Some((name, when)) = line.rsplit_once(' ') else {
                continue;
            };
            let old = when.parse::<u64>().is_ok_and(|t| now.as_secs() > t + SESSION_AGE.as_secs());
            if old && name != self.reference {
                let _ = self.git(&["update-ref", "-d", name]).await;
            }
        }
        let _ = self.git(&["gc", "--auto", "--quiet"]).await;
    }

    async fn create(&self) -> Result<(), String> {
        fs::create_dir_all(&self.git_dir).map_err(|e| e.to_string())?;
        let mut init = Command::new("git");
        init.args(["init", "--quiet", "--bare"]).arg(&self.git_dir);
        init.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
        output(&mut init, "init", None).await.and_then(|o| succeeded(o, "init"))?;
        let mut exclude = EXCLUDE.to_owned();
        // eidra's own data, when the workspace holds it: its data directory, or else this root.
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        let inside = [root.parent(), Some(root.as_path())].into_iter().flatten().find_map(|dir| {
            dir.strip_prefix(&self.workspace).ok().filter(|d| !d.as_os_str().is_empty())
        });
        if let Some(inside) = inside {
            exclude.push_str(&format!("/{}/\n", escape(&inside.to_string_lossy())));
        }
        let info = self.git_dir.join("info");
        fs::create_dir_all(&info).map_err(|e| e.to_string())?;
        fs::write(info.join("exclude"), exclude).map_err(|e| e.to_string())
    }

    fn remove_old_indexes(&self) {
        let Ok(entries) = fs::read_dir(&self.git_dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > INDEX_AGE));
            if old && entry.file_name().to_string_lossy().starts_with("eidra-index-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// The turns recorded on the session's ref, if it has one.
    async fn load(&mut self) -> Result<(), String> {
        let found = self.output(&["rev-parse", "--verify", "--quiet", &self.reference]).await?;
        if !found.status.success() {
            return Ok(());
        }
        self.last = Some(String::from_utf8_lossy(&found.stdout).trim().to_owned());
        let format = "--format=%H%x00%s%x00%b%x1e";
        let log = self.git(&["log", "--reverse", format, &self.reference]).await?;
        for record in log.split('\x1e') {
            let mut fields = record.trim_start_matches('\n').splitn(3, '\0');
            let (Some(commit), Some(subject)) = (fields.next(), fields.next()) else {
                continue;
            };
            let body = fields.next().unwrap_or_default();
            if let Some((number, label)) = subject.strip_prefix("before ").and_then(turn_number) {
                let label = match self.open_label(body) {
                    Some(label) => label,
                    None => label.strip_prefix(": ").unwrap_or(label).to_owned(),
                };
                let before = commit.to_owned();
                self.next = self.next.max(number + 1);
                self.turns.push(Turn { number, label, before, after: None });
            } else if let Some((number, _)) = subject.strip_prefix("after ").and_then(turn_number) {
                if let Some(turn) = self.turns.last_mut().filter(|t| t.number == number) {
                    turn.after = Some(commit.to_owned());
                }
            } else if let Some((number, _)) = subject.strip_prefix("undid ").and_then(turn_number) {
                self.turns.retain(|t| t.number != number);
            }
        }
        // A turn cut off by a crash has nothing to undo against.
        self.turns.retain(|t| t.after.is_some());
        Ok(())
    }

    /// The session's turns, oldest first, without those undone.
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Why checkpoints are off, if they are.
    pub fn off(&self) -> Option<&str> {
        self.off.as_deref()
    }

    /// Records the workspace before a turn that begins with the message `label`. An error, the
    /// first time one happens, says why checkpoints are now off.
    pub async fn begin(&mut self, label: &str) -> Result<(), String> {
        if self.off.is_some() {
            return Ok(());
        }
        let number = self.next;
        let label = label.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
        let label = crate::display::clip(label, 60);
        // Sealed, the label goes into the message's body, and the subject names the turn alone.
        let message = match &self.sealed {
            Some(sealed) => {
                let label = sealed.vault.seal(Purpose::File, LABEL.as_bytes(), label.as_bytes());
                format!("before {number}\n\n{LABEL}{}", STANDARD.encode(label))
            }
            None => format!("before {number}: {label}"),
        };
        match self.snapshot(&message).await {
            Ok(before) => {
                self.next = number + 1;
                self.turns.push(Turn { number, label, before, after: None });
                Ok(())
            }
            Err(why) => Err(self.stop(why)),
        }
    }

    /// Records the workspace after the turn `begin` began.
    pub async fn end(&mut self) -> Result<(), String> {
        let Some(number) = self.turns.last().filter(|t| t.after.is_none()).map(|t| t.number) else {
            return Ok(());
        };
        if self.off.is_some() {
            self.turns.pop();
            return Ok(());
        }
        match self.snapshot(&format!("after {number}")).await {
            Ok(after) => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.after = Some(after);
                }
                Ok(())
            }
            Err(why) => {
                self.turns.pop();
                Err(self.stop(why))
            }
        }
    }

    fn stop(&mut self, why: String) -> String {
        self.off = Some(why.clone());
        why
    }

    /// What changed in the workspace from the start of the last turn to now, or of the first turn
    /// of the session with `all`: the turn's number and a diff, with a summary of files first.
    pub async fn diff(&mut self, all: bool) -> Result<Option<(usize, String)>, String> {
        if let Some(why) = &self.off {
            return Err(why.clone());
        }
        let turn = if all { self.turns.first() } else { self.turns.last() };
        let Some(Turn { number, before, .. }) = turn.cloned() else {
            return Ok(None);
        };
        let now = self.tree().await?;
        let diff = if self.sealed.is_some() {
            // Compared through eidra's filter, which opens them. git's own summary would count
            // the sealed bytes, as a binary file's: this one counts the opened patch's lines.
            let args = ["diff", "--no-color", "--no-ext-diff", "--textconv", "--patch"];
            let patch = self.git(&[&args[..], &[&before, &now]].concat()).await?;
            format!("{}{patch}", summary(&patch))
        } else {
            let args = ["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--stat=100"];
            self.git(&[&args[..], &["--patch", &before, &now]].concat()).await?
        };
        Ok(Some((number, diff)))
    }

    /// A turn's label, sealed in its message's body, if it was and it opens.
    fn open_label(&self, body: &str) -> Option<String> {
        let sealed = &self.sealed.as_ref()?.vault;
        let line = body.lines().find_map(|line| line.strip_prefix(LABEL))?;
        let label = sealed.open(Purpose::File, LABEL.as_bytes(), &STANDARD.decode(line).ok()?);
        String::from_utf8(label.ok()?).ok()
    }

    /// Puts back the files the latest turn changed, the turns that changed nothing aside. A file
    /// changed again since the turn ended is left as it is. None if no turn is left to undo.
    pub async fn undo(&mut self) -> Result<Option<Undone>, String> {
        if let Some(why) = &self.off {
            return Err(why.clone());
        }
        while let Some(turn) = self.turns.last().cloned() {
            let Some(after) = turn.after.clone() else {
                return Err("the turn has not ended".into());
            };
            let args = ["diff", "--name-status", "--no-renames", "-z", &turn.before, &after];
            let changes = parse_name_status(&self.git(&args).await?);
            if changes.is_empty() {
                self.turns.pop();
                continue;
            }
            let now = self.tree().await?;
            let since =
                self.git(&["diff", "--name-only", "--no-renames", "-z", &after, &now]).await?;
            let since: Vec<&str> = since.split('\0').filter(|p| !p.is_empty()).collect();
            // A path changed after the turn, or with something changed at a path above or below
            // it (a file that became a directory, or the other way), is left as it is: putting
            // it back would clear what is in the way.
            let touched =
                |path: &str| since.iter().any(|s| *s == path || nested(s, path) || nested(path, s));
            let mut undone =
                Undone { turn: turn.number, label: turn.label.clone(), ..Undone::default() };
            let mut made = Vec::new();
            for (status, path) in changes {
                if touched(&path) {
                    undone.kept.push(path);
                } else if status == 'A' {
                    made.push(path);
                } else {
                    undone.restored.push(path);
                }
            }
            for chunk in undone.restored.chunks(100) {
                let paths: Vec<&str> = chunk.iter().map(String::as_str).collect();
                self.git(&[&["checkout", &turn.before, "--"][..], &paths].concat()).await?;
            }
            // A file is new to the snapshots when the turn made it, but also when the turn stopped
            // ignoring it (a .gitignore replaced, say): under the rules put back, an ignored file
            // stays. So does one behind a symbolic link, which may lead out of the workspace.
            let (made, behind_links): (Vec<String>, Vec<String>) =
                made.into_iter().partition(|path| self.removable(path));
            undone.kept.extend(behind_links);
            let ignored = self.ignored(&made).await;
            for path in made {
                if ignored.contains(&path) {
                    undone.kept.push(path);
                } else {
                    self.remove(&path);
                    undone.removed.push(path);
                }
            }
            undone.kept.sort();
            self.snapshot(&format!("undid {}", turn.number)).await?;
            self.turns.pop();
            return Ok(Some(undone));
        }
        Ok(None)
    }

    /// Which of `paths` the ignore files on disk ignore now. When git cannot say, all of them:
    /// a file kept by mistake costs less than one removed.
    async fn ignored(&self, paths: &[String]) -> HashSet<String> {
        let mut ignored = HashSet::new();
        for chunk in paths.chunks(100) {
            let args = ["check-ignore", "--no-index", "-z", "--stdin"];
            // check-ignore takes paths, not pathspecs, and refuses the literal setting.
            let mut command = self.command(&args);
            command.env_remove("GIT_LITERAL_PATHSPECS");
            let input: Vec<u8> =
                chunk.iter().flat_map(|p| [p.as_bytes(), b"\0"]).flatten().copied().collect();
            match output(&mut command, "check-ignore", Some(&input)).await {
                // 1 is none of them ignored.
                Ok(output) if matches!(output.status.code(), Some(0 | 1)) => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    ignored.extend(text.split('\0').filter(|p| !p.is_empty()).map(str::to_owned));
                }
                _ => ignored.extend(chunk.iter().cloned()),
            }
        }
        ignored
    }

    /// Whether `path` can be removed without following a symbolic link: no directory between the
    /// workspace and it may be one.
    fn removable(&self, path: &str) -> bool {
        let mut dir = self.workspace.clone();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            dir.push(part);
            if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
                return false;
            }
        }
        true
    }

    /// Removes a file the turn made, and the directories it made for it once they are empty.
    fn remove(&self, path: &str) {
        let full = self.workspace.join(path);
        let _ = fs::remove_file(&full);
        let mut dir = full.parent();
        while let Some(d) = dir.filter(|d| d.starts_with(&self.workspace) && *d != self.workspace) {
            if fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    /// Records the workspace as a commit after the last one, on the session's ref.
    async fn snapshot(&mut self, message: &str) -> Result<String, String> {
        let tree = self.tree().await?;
        let mut args = vec!["commit-tree", &tree, "-m", message];
        let last = self.last.clone();
        if let Some(parent) = &last {
            args.extend(["-p", parent]);
        }
        let commit = self.git(&args).await?.trim().to_owned();
        self.git(&["update-ref", &self.reference, &commit]).await?;
        self.last = Some(commit.clone());
        Ok(commit)
    }

    /// The workspace as it is now, as a tree in the repository.
    async fn tree(&mut self) -> Result<String, String> {
        self.ready().await?;
        self.exclude_large_files().await?;
        // Only this session uses its index, one command at a time: a lock is a killed git's.
        let _ = fs::remove_file(self.index.with_extension("lock"));
        let add = self.output(&["add", "--all", "--ignore-errors", "--", "."]).await?;
        // 1 is files it could not read, which it leaves out.
        if !matches!(add.status.code(), Some(0 | 1)) {
            return Err(failure("add", &add));
        }
        Ok(self.git(&["write-tree"]).await?.trim().to_owned())
    }

    /// Adds new files over the size limit to the repository's exclude file.
    async fn exclude_large_files(&self) -> Result<(), String> {
        let others = self.git(&["ls-files", "-z", "--others", "--exclude-standard"]).await?;
        let others: Vec<&str> = others.split('\0').filter(|p| !p.is_empty()).collect();
        if others.len() > MOST_NEW_FILES {
            return Err(format!(
                "the workspace has {} new files outside its ignore files, too many to snapshot \
                 each turn",
                others.len()
            ));
        }
        let large: Vec<String> = others
            .into_iter()
            .filter(|path| !path.contains('\n'))
            .filter(|path| {
                fs::symlink_metadata(self.workspace.join(path))
                    .is_ok_and(|m| m.is_file() && m.len() > LARGEST_FILE)
            })
            .map(|path| format!("/{}\n", escape(path)))
            .collect();
        if large.is_empty() {
            return Ok(());
        }
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.git_dir.join("info/exclude"))
            .map_err(|e| e.to_string())?;
        file.write_all(large.concat().as_bytes()).map_err(|e| e.to_string())
    }

    async fn git(&self, args: &[&str]) -> Result<String, String> {
        let output = self.output(args).await?;
        let output = succeeded(output, args[0])?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs git on the workspace, with this repository and the session's index.
    async fn output(&self, args: &[&str]) -> Result<Output, String> {
        output(&mut self.command(args), args[0], None).await
    }

    /// git on the workspace, with this repository, the session's index, and the settings eidra's
    /// snapshots need.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.arg("--git-dir").arg(&self.git_dir).arg("--work-tree").arg(&self.workspace);
        // The user's settings apply, except those that would change what is recorded or run
        // something of theirs.
        for setting in [
            "core.autocrlf=false",
            "core.fsmonitor=false",
            "core.hooksPath=/dev/null",
            "core.quotePath=false",
            "commit.gpgSign=false",
            "advice.addEmbeddedRepo=false",
        ] {
            command.args(["-c", setting]);
        }
        if let Some(sealed) = &self.sealed {
            // git runs these through the shell.
            let program = shell_quote(&sealed.program.to_string_lossy());
            command
                .arg("-c")
                .arg(format!("filter.eidra.process={program} snapshot-filter process"));
            command.args(["-c", "filter.eidra.required=true"]);
            command
                .arg("-c")
                .arg(format!("diff.eidra.textconv={program} snapshot-filter textconv"));
            command.env(KEY_ENV, sealed.key.as_str());
        }
        command
            .args(args)
            .current_dir(&self.workspace)
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .envs(IDENTITY)
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_NAMESPACE");
        command
    }
}

/// The name of the repository, under the checkpoints' root, that keeps the snapshots of
/// `workspace`, a canonical path.
pub fn repository_name(workspace: &Path) -> String {
    let name = workspace.file_name().map(|n| n.to_string_lossy().into_owned());
    let hash = fnv1a(workspace.as_os_str().as_bytes()) as u32;
    format!("{}-{hash:08x}", name.unwrap_or_default())
}

/// What `forget_sessions` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// Sessions whose snapshots went.
    pub sessions: usize,
    /// Repositories whose pruning waits, since an open session uses them.
    pub pending: usize,
}

/// Lets go of the snapshots of the sessions `ids` in every repository under `root`, and has git
/// remove at once, rather than after its two weeks, every object nothing refers to any more:
/// theirs, and what snapshots that expired before left behind (§14). A repository named in
/// `busy` is used by an open session, which may be adding a snapshot as it goes: its refs go,
/// and it is marked to be pruned by a later erase.
pub async fn forget_sessions(
    root: &Path,
    ids: &HashSet<String>,
    busy: &HashSet<String>,
) -> Result<Forgotten, String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(Forgotten::default());
    };
    let mut forgotten = Forgotten::default();
    for entry in entries.flatten() {
        let git_dir = entry.path();
        if !git_dir.join("HEAD").exists() {
            continue;
        }
        let refs = repository_git(&git_dir, &["for-each-ref", "--format=%(refname)"], None).await?;
        let gone: Vec<&str> = refs
            .lines()
            .filter(|r| r.strip_prefix("refs/eidra/").is_some_and(|id| ids.contains(id)))
            .collect();
        if !gone.is_empty() {
            let input: String = gone.iter().map(|r| format!("delete {r}\n")).collect();
            repository_git(&git_dir, &["update-ref", "--stdin"], Some(input.as_bytes())).await?;
            forgotten.sessions += gone.len();
        }
        if busy.contains(&entry.file_name().to_string_lossy().into_owned()) {
            if !gone.is_empty() {
                fs::write(git_dir.join(PRUNE_PENDING), "").map_err(|e| e.to_string())?;
            }
            forgotten.pending += usize::from(git_dir.join(PRUNE_PENDING).exists());
            continue;
        }
        // No session is open on it, so no index is in use. An object only an index holds would
        // go with the prune and leave the index naming what is gone: the indexes go first, and
        // each session makes its own again, the same, at its next snapshot.
        for entry in fs::read_dir(&git_dir).map_err(|e| e.to_string())?.flatten() {
            if entry.file_name().to_string_lossy().starts_with("eidra-index-") {
                let _ = fs::remove_file(entry.path());
            }
        }
        repository_git(&git_dir, &["gc", "--prune=now", "--quiet"], None).await?;
        let _ = fs::remove_file(git_dir.join(PRUNE_PENDING));
    }
    Ok(forgotten)
}

/// Repositories under `root` whose pruning an erase put off.
pub fn prune_pending(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    entries.flatten().filter(|entry| entry.path().join(PRUNE_PENDING).exists()).count()
}

/// git's `--stat` summary of a patch, counted from its lines.
fn summary(patch: &str) -> String {
    let mut files: Vec<(String, usize, usize)> = Vec::new();
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            let path = paths.rsplit_once(" b/").map_or(paths, |(_, b)| b);
            files.push((path.to_owned(), 0, 0));
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if let Some(file) = files.last_mut().filter(|_| in_hunk) {
            // Lines in a hunk only: a removed line starting "-- " reads like the "--- " header.
            match line.as_bytes().first() {
                Some(b'+') => file.1 += 1,
                Some(b'-') => file.2 += 1,
                _ => {}
            }
        }
    }
    if files.is_empty() {
        return String::new();
    }
    let width = files.iter().map(|(path, ..)| path.chars().count()).max().unwrap_or(0);
    let most = files.iter().map(|(_, added, removed)| added + removed).max().unwrap_or(0);
    let digits = most.to_string().len();
    // The bars fit in 40 columns, as git's do in a narrow terminal.
    let scale = |n: usize| if most > 40 { (n * 40).div_ceil(most) } else { n };
    let mut out = String::new();
    let (mut added, mut removed) = (0, 0);
    for (path, plus, minus) in &files {
        let bar = format!("{}{}", "+".repeat(scale(*plus)), "-".repeat(scale(*minus)));
        out.push_str(&format!(" {path:<width$} | {:>digits$} {bar}\n", plus + minus));
        added += plus;
        removed += minus;
    }
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    out.push_str(&format!(
        " {} changed, {}, {}\n\n",
        plural(files.len(), "file", "files"),
        plural(added, "insertion(+)", "insertions(+)"),
        plural(removed, "deletion(-)", "deletions(-)")
    ));
    out
}

/// git on a snapshot repository alone, with no work tree.
async fn repository_git(
    git_dir: &Path,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("--git-dir").arg(git_dir).args(["-c", "core.hooksPath=/dev/null"]).args(args);
    command.env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE").env_remove("GIT_NAMESPACE");
    let output = succeeded(output(&mut command, args[0], input).await?, args[0])?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs git, with `input` on its stdin, and returns what it wrote.
async fn output(command: &mut Command, what: &str, input: Option<&[u8]>) -> Result<Output, String> {
    let stdin = if input.is_some() { Stdio::piped() } else { Stdio::null() };
    command.stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "git is not installed".to_owned(),
        _ => format!("running git: {e}"),
    })?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // Short lists of paths, which fit in the pipe while git waits to read them.
        let _ = stdin.write_all(input).await;
    }
    tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("git {what} took over {} s", TIMEOUT.as_secs()))?
        .map_err(|e| format!("running git: {e}"))
}

fn succeeded(output: Output, what: &str) -> Result<Output, String> {
    if output.status.success() { Ok(output) } else { Err(failure(what, &output)) }
}

fn failure(what: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let why = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("it failed");
    format!("git {what}: {}", why.trim())
}

/// `text` quoted for the shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Whether `inner` is below the directory `outer`.
fn nested(inner: &str, outer: &str) -> bool {
    inner.len() > outer.len() && inner.starts_with(outer) && inner.as_bytes()[outer.len()] == b'/'
}

/// `3: label` or `3` as the turn's number and the rest.
fn turn_number(rest: &str) -> Option<(usize, &str)> {
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    Some((rest[..end].parse().ok()?, &rest[end..]))
}

/// `git diff --name-status -z` output as each path with its status letter.
fn parse_name_status(text: &str) -> Vec<(char, String)> {
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        changes.push((status.chars().next().unwrap_or('M'), path.to_owned()));
    }
    changes
}

/// A path as an ignore-file pattern that matches it alone.
fn escape(path: &str) -> String {
    let mut escaped = String::new();
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ' ' | '!' | '#') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("eidra-cp-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("work/src")).unwrap();
            fs::create_dir_all(dir.join("data")).unwrap();
            Scratch(dir)
        }

        fn work(&self) -> PathBuf {
            self.0.join("work")
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.work().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn read(&self, path: &str) -> Option<String> {
            fs::read_to_string(self.work().join(path)).ok()
        }

        async fn open(&self, session: &str) -> Checkpoints {
            Checkpoints::resume(&self.0.join("data"), &self.work(), session).await.unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn has_git() -> bool {
        std::process::Command::new("git").arg("--version").output().is_ok()
    }

    #[tokio::test]
    async fn undo_puts_back_what_a_turn_changed_and_leaves_what_changed_since() {
        if !has_git() {
            return eprintln!("no git: skipped");
        }
        let s = Scratch::new("undo");
        s.write("src/a.py", "a = 1\n");
        s.write("src/b.py", "b = 1\n");
        s.write("gone.txt", "kept until the turn\n");
        let mut checkpoints = s.open("s1").await;

        checkpoints.begin("first: change things\nmore").await.unwrap();
        s.write("src/a.py", "a = 2\n");
        s.write("src/b.py", "b = 2\n");
        s.write("src/new/c.py", "c = 1\n");
        s.write("we*ird name.txt", "made\n");
        fs::remove_file(s.work().join("gone.txt")).unwrap();
        checkpoints.end().await.unwrap();
        // The user edits b.py after the turn.
        s.write("src/b.py", "b = 3\n");
        // A turn that changes nothing is passed over.
        checkpoints.begin("second").await.unwrap();
        checkpoints.end().await.unwrap();

        let (turn, diff) = checkpoints.diff(true).await.unwrap().unwrap();
        assert_eq!(turn, 1);
        assert!(diff.contains("-a = 1\n+a = 2") && diff.contains("src/new/c.py"), "{diff}");

        let undone = checkpoints.undo().await.unwrap().unwrap();
        assert_eq!(
            undone,
            Undone {
                turn: 1,
                label: "first: change things".into(),
                restored: vec!["gone.txt".into(), "src/a.py".into()],
                removed: vec!["src/new/c.py".into(), "we*ird name.txt".into()],
                kept: vec!["src/b.py".into()],
            }
        );
        assert_eq!(s.read("src/a.py").as_deref(), Some("a = 1\n"));
        assert_eq!(s.read("src/b.py").as_deref(), Some("b = 3\n"));
        assert_eq!(s.read("gone.txt").as_deref(), Some("kept until the turn\n"));
        assert!(!s.work().join("src/new").exists() && s.work().join("src").exists());
        assert!(s.read("we*ird name.txt").is_none());
        assert_eq!(checkpoints.undo().await.unwrap(), None);
    }

    #[tokio::test]
    async fn undo_puts_back_exact_bytes_whatever_the_projects_attributes() {
        if !has_git() {
            return eprintln!("no git: skipped");
        }
        let s = Scratch::new("bytes");
        // Rules that change a file on its way into a snapshot or back out of one.
        s.write(
            ".gitattributes",
            "*.bat text eol=crlf\n*.py text=auto\n*.txt ident\n*.ipynb filter=strip\n",
        );
        let files = [
            ("run.bat", "@echo off\necho written with LF\n"),
            ("src/a.py", "a = 1\r\nb = 2\r\n"),
            ("id.txt", "$Id: as typed $\n"),
            ("nb.ipynb", "{\"outputs\": [\"kept\"]}\n"),
        ];
        for (path, text) in files {
            s.write(path, text);
        }
        let mut checkpoints = s.open("s1").await;
        checkpoints.ready().await.unwrap();
        // A filter of the user's, which leaves the outputs out.
        checkpoints.git(&["config", "filter.strip.clean", "sed s/kept//"]).await.unwrap();
        checkpoints.begin("change them").await.unwrap();
        for (path, _) in files {
            s.write(path, "changed\n");
        }
        checkpoints.end().await.unwrap();
        checkpoints.undo().await.unwrap().unwrap();
        for (path, text) in files {
            assert_eq!(s.read(path).as_deref(), Some(text), "{path}");
        }
    }

    #[tokio::test]
    async fn a_resumed_session_finds_its_turns() {
        if !has_git() {
            return eprintln!("no git: skipped");
        }
        let s = Scratch::new("resume");
        s.write("a.txt", "1\n");
        let mut first = s.open("s1").await;
        for (n, text) in [(1, "2\n"), (2, "3\n"), (3, "4\n")] {
            first.begin(&format!("turn {n}")).await.unwrap();
            s.write("a.txt", text);
            first.end().await.unwrap();
        }
        first.undo().await.unwrap().unwrap();
        // Another session in the same workspace has turns of its own.
        let other = s.open("s2").await;
        assert!(other.turns().is_empty());

        let resumed = s.open("s1").await;
        let turns: Vec<(usize, &str)> =
            resumed.turns().iter().map(|t| (t.number, t.label.as_str())).collect();
        assert_eq!(turns, [(1, "turn 1"), (2, "turn 2")]);
        assert_eq!(s.read("a.txt").as_deref(), Some("3\n"));
    }

    #[tokio::test]
    async fn large_files_and_caches_are_left_out() {
        if !has_git() {
            return eprintln!("no git: skipped");
        }
        let s = Scratch::new("large");
        s.write("big.bin", &"x".repeat(LARGEST_FILE as usize + 1));
        s.write(".venv/lib/site.py", "x = 1\n");
        s.write("src/__pycache__/a.pyc", "x");
        s.write("src/a.py", "a = 1\n");
        let mut checkpoints = s.open("s1").await;
        checkpoints.begin("go").await.unwrap();
        s.write("src/a.py", "a = 2\n");
        checkpoints.end().await.unwrap();
        let files = checkpoints.git(&["ls-files"]).await.unwrap();
        assert_eq!(files, "src/a.py\n");
    }

    #[tokio::test]
    async fn eidra_data_in_the_workspace_is_left_out() {
        if !has_git() {
            return eprintln!("no git: skipped");
        }
        let s = Scratch::new("inside");
        s.write(".eidra/sessions/s1.jsonl", "{}\n");
        s.write("a.txt", "1\n");
        let root = s.work().join(".eidra/checkpoints");
        let mut checkpoints = Checkpoints::new(&root, &s.work(), "s1").unwrap();
        checkpoints.begin("go").await.unwrap();
        checkpoints.end().await.unwrap();
        assert_eq!(checkpoints.git(&["ls-files"]).await.unwrap(), "a.txt\n");
    }

    #[tokio::test]
    async fn undo_never_deletes_through_a_link_nor_what_the_turn_did_not_make() {
        if !has_git() {
            return eprintln!("no git: skipped");
        }
        let s = Scratch::new("safe");
        // Outside the workspace, reached through a link in it.
        let outside = s.0.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("notes.md"), "keep me\n").unwrap();
        std::os::unix::fs::symlink(&outside, s.work().join("docs")).unwrap();
        // Ignored before the turn: the turn's new .gitignore stops ignoring it.
        s.write(".gitignore", "*.local\n");
        s.write("db.local", "secret\n");
        // A file the turn deletes, where the user then makes a directory.
        s.write("build", "old\n");
        let mut checkpoints = s.open("s1").await;

        checkpoints.begin("tidy up").await.unwrap();
        fs::remove_file(s.work().join("docs")).unwrap();
        s.write("docs/notes.md", "keep me\n");
        s.write(".gitignore", "__pycache__/\n");
        fs::remove_file(s.work().join("build")).unwrap();
        checkpoints.end().await.unwrap();
        s.write("build/mine.txt", "the user's\n");

        let undone = checkpoints.undo().await.unwrap().unwrap();
        assert_eq!(undone.restored, [".gitignore", "docs"]);
        assert_eq!(undone.kept, ["build", "db.local", "docs/notes.md"]);
        assert!(undone.removed.is_empty());
        assert_eq!(fs::read_to_string(outside.join("notes.md")).unwrap(), "keep me\n");
        assert_eq!(s.read("db.local").as_deref(), Some("secret\n"));
        assert_eq!(s.read("build/mine.txt").as_deref(), Some("the user's\n"));
        assert!(fs::symlink_metadata(s.work().join("docs")).unwrap().file_type().is_symlink());
    }

    #[tokio::test]
    async fn turn_numbers_are_never_used_twice() {
        if !has_git() {
            return eprintln!("no git: skipped");
        }
        let s = Scratch::new("numbers");
        s.write("a.txt", "1\n");
        let mut checkpoints = s.open("s1").await;
        for (label, text) in [("one", "2\n"), ("two", "3\n")] {
            checkpoints.begin(label).await.unwrap();
            s.write("a.txt", text);
            checkpoints.end().await.unwrap();
        }
        assert_eq!(checkpoints.undo().await.unwrap().unwrap().turn, 2);
        checkpoints.begin("three").await.unwrap();
        checkpoints.end().await.unwrap();
        let numbers: Vec<usize> = checkpoints.turns().iter().map(|t| t.number).collect();
        assert_eq!(numbers, [1, 3]);
        let resumed = s.open("s1").await;
        let numbers: Vec<usize> = resumed.turns().iter().map(|t| t.number).collect();
        assert_eq!(numbers, [1, 3]);
    }

    #[tokio::test]
    async fn old_sessions_are_let_go() {
        if !has_git() {
            return eprintln!("no git: skipped");
        }
        let s = Scratch::new("old");
        s.write("a.txt", "1\n");
        let mut old = s.open("old").await;
        old.begin("long ago").await.unwrap();
        old.end().await.unwrap();
        // Its last snapshot, made again as if long ago.
        let tree = old.git(&["write-tree"]).await.unwrap();
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&old.git_dir)
            .args(["commit-tree", tree.trim(), "-m", "after 1"])
            .env("GIT_COMMITTER_DATE", "@1000000000 +0000")
            .envs(IDENTITY);
        let backdated = output(&mut command, "commit-tree", None).await.unwrap();
        let backdated = String::from_utf8_lossy(&backdated.stdout).trim().to_owned();
        old.git(&["update-ref", "refs/eidra/old", &backdated]).await.unwrap();

        let mut new = s.open("new").await;
        new.begin("now").await.unwrap();
        new.end().await.unwrap();
        let refs = new.git(&["for-each-ref", "--format=%(refname)", "refs/eidra/"]).await.unwrap();
        assert_eq!(refs, "refs/eidra/new\n");
    }

    #[test]
    fn the_summary_of_a_patch_counts_its_hunks_lines() {
        let patch = "diff --git a/q.sql b/q.sql\nindex 1..2 100644\n--- a/q.sql\n+++ b/q.sql\n\
                     @@ -1,2 +1,2 @@\n--- a comment\n+select 2;\n keep\n\
                     diff --git a/new.py b/new.py\n--- /dev/null\n+++ b/new.py\n@@ -0,0 +1 @@\n\
                     +print()\n";
        assert_eq!(
            summary(patch),
            " q.sql  | 2 +-\n new.py | 1 +\n 2 files changed, 2 insertions(+), 1 deletion(-)\n\n"
        );
        assert_eq!(summary(""), "");
    }

    #[test]
    fn reads_what_git_writes() {
        assert_eq!(
            parse_name_status("M\0src/a.py\0A\0b c.txt\0D\0gone\0"),
            [('M', "src/a.py".into()), ('A', "b c.txt".into()), ('D', "gone".into())]
        );
        assert_eq!(turn_number("12: fix it"), Some((12, ": fix it")));
        assert_eq!(turn_number("x"), None);
        assert_eq!(escape("a b/*[x]?.txt"), r"a\ b/\*\[x]\?.txt");
    }
}
