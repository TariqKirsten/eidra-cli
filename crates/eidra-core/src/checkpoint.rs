//! Checkpoints (docs/DESIGN.md §14): a git repository of eidra's own records the workspace before
//! and after each turn of an interactive session, so that `/undo` can put back what a turn
//! changed and `/diff` can show it. It sees what commands changed as well as what the file tools
//! did, and never touches the project's own repository, index or branches.
//!
//! The repository is bare, one per workspace, under eidra's data directory. Each snapshot is a
//! commit on a ref of the session's own, `refs/eidra/<session>`, whose subjects say which turn it
//! belongs to, so a resumed session finds its turns again. The project's ignore files apply, and
//! a few defaults for caches and environments; a new file over 8 MiB is left out for good.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, SystemTime};

use tokio::process::Command;

use crate::text::fnv1a;

/// The longest one git command may take. Past it, checkpoints are off for the session.
const TIMEOUT: Duration = Duration::from_secs(20);
/// A new file larger than this is left out of every snapshot.
const LARGEST_FILE: u64 = 8 << 20;
/// New files one snapshot takes at most: more means a workspace too big to snapshot each turn.
const MOST_NEW_FILES: usize = 20_000;
/// A session's index is removed this long after it was last used.
const INDEX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
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
    /// The repository has been made, if it had to be.
    ready: bool,
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
        let name = workspace.file_name().map(|n| n.to_string_lossy().into_owned());
        let hash = fnv1a(workspace.as_os_str().as_bytes()) as u32;
        let git_dir = root.join(format!("{}-{hash:08x}", name.unwrap_or_default()));
        Ok(Checkpoints {
            root: root.to_path_buf(),
            index: git_dir.join(format!("eidra-index-{session}")),
            git_dir,
            workspace,
            reference: format!("refs/eidra/{session}"),
            turns: Vec::new(),
            last: None,
            off: None,
            ready: false,
        })
    }

    /// The checkpoints of a session that may have turns already, read from its ref.
    pub async fn resume(root: &Path, workspace: &Path, session: &str) -> Result<Self, String> {
        let mut checkpoints = Self::new(root, workspace, session)?;
        if checkpoints.git_dir.join("HEAD").exists() {
            checkpoints.load().await?;
        }
        Ok(checkpoints)
    }

    /// Makes the repository if there is none yet.
    async fn ready(&mut self) -> Result<(), String> {
        if self.ready {
            return Ok(());
        }
        if !self.git_dir.join("HEAD").exists() {
            self.create().await?;
        }
        self.remove_old_indexes();
        self.ready = true;
        Ok(())
    }

    async fn create(&self) -> Result<(), String> {
        fs::create_dir_all(&self.git_dir).map_err(|e| e.to_string())?;
        let mut init = Command::new("git");
        init.args(["init", "--quiet", "--bare"]).arg(&self.git_dir);
        init.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
        output(&mut init, "init").await.and_then(|o| succeeded(o, "init"))?;
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
        let log = self.git(&["log", "--reverse", "--format=%H %s", &self.reference]).await?;
        for line in log.lines() {
            let Some((commit, subject)) = line.split_once(' ') else {
                continue;
            };
            if let Some((number, label)) = subject.strip_prefix("before ").and_then(turn_number) {
                let label = label.strip_prefix(": ").unwrap_or(label).to_owned();
                let before = commit.to_owned();
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
        let number = self.turns.iter().map(|t| t.number).max().unwrap_or(0) + 1;
        let label = label.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
        let label = crate::display::clip(label, 60);
        match self.snapshot(&format!("before {number}: {label}")).await {
            Ok(before) => {
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
        let args = ["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--stat=100"];
        let diff = self.git(&[&args[..], &["--patch", &before, &now]].concat()).await?;
        Ok(Some((number, diff)))
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
            let since: HashSet<&str> = since.split('\0').filter(|p| !p.is_empty()).collect();
            let mut undone =
                Undone { turn: turn.number, label: turn.label.clone(), ..Undone::default() };
            for (status, path) in changes {
                if since.contains(path.as_str()) {
                    undone.kept.push(path);
                } else if status == 'A' {
                    undone.removed.push(path);
                } else {
                    undone.restored.push(path);
                }
            }
            for chunk in undone.restored.chunks(100) {
                let paths: Vec<&str> = chunk.iter().map(String::as_str).collect();
                self.git(&[&["checkout", &turn.before, "--"][..], &paths].concat()).await?;
            }
            for path in &undone.removed {
                self.remove(path);
            }
            self.snapshot(&format!("undid {}", turn.number)).await?;
            self.turns.pop();
            return Ok(Some(undone));
        }
        Ok(None)
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
    async fn snapshot(&mut self, subject: &str) -> Result<String, String> {
        let tree = self.tree().await?;
        let mut args = vec!["commit-tree", &tree, "-m", subject];
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
        command
            .args(args)
            .current_dir(&self.workspace)
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .envs(IDENTITY)
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_NAMESPACE");
        output(&mut command, args[0]).await
    }
}

async fn output(command: &mut Command, what: &str) -> Result<Output, String> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "git is not installed".to_owned(),
        _ => format!("running git: {e}"),
    })?;
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
