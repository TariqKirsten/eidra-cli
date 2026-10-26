//! `read`, `write` and `edit`.
//!
//! `write` will not replace a file the model has not seen as it now is: it shows the file instead,
//! and the same call goes through next time. `edit` needs no read first, because `old` must match
//! the file exactly; the model often knows a file from grep or cat, and an edit refused for want of
//! a read costs two steps (DESIGN.md §16.1).

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::resolve;
use crate::text::{fnv1a, numbered};

const DEFAULT_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
/// Lines of context shown around an edit.
const CONTEXT: usize = 3;

pub struct Files {
    workspace: PathBuf,
    /// The hash of each file's contents when the model last read or wrote it.
    seen: HashMap<PathBuf, u64>,
}

impl Files {
    pub fn new(workspace: &Path) -> Self {
        Files { workspace: workspace.to_path_buf(), seen: HashMap::new() }
    }

    pub fn read(
        &mut self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<String, String> {
        let abs = resolve(&self.workspace, path);
        if abs.is_dir() {
            return Err(format!("{path} is a directory; list it with bash (ls) or glob"));
        }
        let bytes = fs::read(&abs).map_err(|e| self.failed(path, &abs, format!("{path}: {e}")))?;
        let out = listing(path, &bytes, offset, limit)?;
        self.seen.insert(abs, fnv1a(&bytes));
        Ok(out)
    }

    pub fn write(&mut self, path: &str, content: &str) -> Result<String, String> {
        let abs = resolve(&self.workspace, path);
        if abs.exists() {
            let current =
                fs::read(&abs).map_err(|e| self.failed(path, &abs, format!("{path}: {e}")))?;
            self.check_fresh(path, &abs, &current)?;
        }
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                self.failed(path, &abs, format!("creating {}: {e}", parent.display()))
            })?;
        }
        fs::write(&abs, content).map_err(|e| self.failed(path, &abs, format!("{path}: {e}")))?;
        self.seen.insert(abs, fnv1a(content.as_bytes()));
        Ok(format!("Wrote {path} ({} lines).", content.lines().count()))
    }

    pub fn edit(
        &mut self,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<String, String> {
        let abs = resolve(&self.workspace, path);
        let bytes = fs::read(&abs).map_err(|e| {
            self.failed(
                path,
                &abs,
                match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        format!("{path} does not exist; create it with write")
                    }
                    _ => format!("{path}: {e}"),
                },
            )
        })?;
        // An edit shows only the lines around it, so it keeps the model's view of the whole file
        // current only if that view was current before.
        let fresh = self.seen.get(&abs) == Some(&fnv1a(&bytes));
        let text = String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text"))?;
        if old.is_empty() {
            return Err("old is empty: to add text, include a neighbouring line in old".into());
        }
        if old == new {
            return Err("old and new are the same".into());
        }
        let (old, new) = find_form(&text, old, new).ok_or_else(|| not_found(path, &text, old))?;
        let starts: Vec<usize> = text.match_indices(&*old).map(|(i, _)| i).collect();
        if starts.len() > 1 && !replace_all {
            let lines: Vec<String> =
                starts.iter().map(|&i| line_of(&text, i).to_string()).collect();
            return Err(format!(
                "old occurs {} times in {path} (lines {}); include more surrounding lines to pick one, \
                 or set replace_all",
                starts.len(),
                lines.join(", ")
            ));
        }
        let updated =
            if replace_all { text.replace(&*old, &new) } else { text.replacen(&*old, &new, 1) };
        fs::write(&abs, &updated).map_err(|e| self.failed(path, &abs, format!("{path}: {e}")))?;
        if fresh {
            self.seen.insert(abs, fnv1a(updated.as_bytes()));
        }

        // Show the first replacement in place, numbered as the file now is.
        let first = line_of(&updated, starts[0]);
        let last = first + new.matches('\n').count();
        let lines: Vec<&str> = updated.lines().collect();
        let from = first.saturating_sub(CONTEXT).max(1);
        let to = (last + CONTEXT).min(lines.len());
        let snippet = if from <= to {
            numbered(lines[from - 1..to].iter().copied(), from)
        } else {
            String::new()
        };
        let count = if starts.len() > 1 {
            format!(", {} replacements", starts.len())
        } else {
            String::new()
        };
        Ok(format!("Edited {path}{count}. Lines {from}–{to} now read:\n{snippet}"))
    }

    /// An I/O error on `path`. A path outside the workspace is usually a mistaken absolute path, or
    /// a project the model has imagined (DESIGN.md §16.2), so the error says where the workspace is.
    fn failed(&self, path: &str, abs: &Path, error: String) -> String {
        if abs.starts_with(&self.workspace) {
            error
        } else {
            format!("{error}. {path} is outside the workspace, {}", self.workspace.display())
        }
    }

    /// Refuses to replace a file the model has not seen as it now is, and shows it the file
    /// instead, so that the same write goes through next time.
    fn check_fresh(&mut self, path: &str, abs: &Path, current: &[u8]) -> Result<(), String> {
        let hash = fnv1a(current);
        let why = match self.seen.get(abs) {
            Some(&seen) if seen == hash => return Ok(()),
            Some(_) => "has changed since you last read it",
            None => "already exists and you have not read it",
        };
        self.seen.insert(abs.to_path_buf(), hash);
        let shown = listing(path, current, None, None).unwrap_or_else(|note| format!("({note})\n"));
        Err(format!(
            "{path} {why}, so it was not replaced. It reads:\n{shown}To replace it anyway, call \
             write again."
        ))
    }
}

/// `bytes` as numbered lines: `limit` of them from line `offset`.
fn listing(
    path: &str,
    bytes: &[u8],
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<String, String> {
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(format!("{path} is a binary file ({} bytes)", bytes.len()));
    }
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok(format!("({path} is empty)\n"));
    }
    let start = offset.unwrap_or(1).max(1) as usize;
    if start > lines.len() {
        return Err(format!("{path} has {} lines; offset {start} is past the end", lines.len()));
    }
    let end = (start - 1 + limit.map_or(DEFAULT_LINES, |l| l.max(1) as usize)).min(lines.len());
    let shown: Vec<Cow<str>> = lines[start - 1..end].iter().map(|line| clip(line)).collect();
    let mut out = numbered(shown.iter().map(|line| &**line), start);
    if end < lines.len() {
        out.push_str(&format!(
            "({} more lines; continue with offset {})\n",
            lines.len() - end,
            end + 1
        ));
    }
    Ok(out)
}

/// A line cut to a readable length.
fn clip(line: &str) -> Cow<'_, str> {
    if line.chars().count() <= MAX_LINE_CHARS {
        return Cow::Borrowed(line);
    }
    let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    Cow::Owned(format!("{cut}… (line cut at {MAX_LINE_CHARS} characters)"))
}

/// The form of `old` (and the matching `new`) that occurs in `text`: as given, without leading and
/// trailing newlines (where the tool-call format is ambiguous), or with CRLF line endings.
fn find_form<'a>(text: &str, old: &'a str, new: &'a str) -> Option<(Cow<'a, str>, Cow<'a, str>)> {
    if text.contains(old) {
        return Some((Cow::Borrowed(old), Cow::Borrowed(new)));
    }
    let trimmed = old.trim_matches('\n');
    if !trimmed.is_empty() && trimmed != old && text.contains(trimmed) {
        return Some((Cow::Borrowed(trimmed), Cow::Borrowed(new.trim_matches('\n'))));
    }
    if text.contains("\r\n") && !old.contains('\r') {
        let crlf = old.replace('\n', "\r\n");
        if text.contains(&crlf) {
            return Some((Cow::Owned(crlf), Cow::Owned(new.replace('\n', "\r\n"))));
        }
    }
    None
}

/// Why `old` was not found, and where to look.
fn not_found(path: &str, text: &str, old: &str) -> String {
    let Some(first) = old.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return format!("old was not found in {path}");
    };
    let hits: Vec<String> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == first)
        .map(|(i, _)| (i + 1).to_string())
        .take(5)
        .collect();
    if hits.is_empty() {
        format!(
            "old was not found in {path}, and neither was its first line; read the file again, \
             it may not be what you remember"
        )
    } else {
        format!(
            "old was not found in {path}. Its first line appears (ignoring indentation) at line {}; \
             check the whitespace and the lines after it against the file",
            hits.join(", ")
        )
    }
}

/// The 1-based line that byte `index` is on.
fn line_of(text: &str, index: usize) -> usize {
    text[..index].matches('\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eidra-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_numbered_lines_in_parts() {
        let dir = workspace("read");
        fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut files = Files::new(&dir);
        assert_eq!(
            files.read("a.txt", None, None).unwrap(),
            "     1\tone\n     2\ttwo\n     3\tthree\n"
        );
        assert_eq!(
            files.read("a.txt", Some(2), Some(1)).unwrap(),
            "     2\ttwo\n(1 more lines; continue with offset 3)\n"
        );
        assert!(files.read("a.txt", Some(9), None).unwrap_err().contains("past the end"));
        let missing = files.read("missing.txt", None, None).unwrap_err();
        assert!(!missing.contains("outside the workspace"), "{missing}");
        let outside = files.read("/eidra-no-such-dir/report.py", None, None).unwrap_err();
        assert!(
            outside.ends_with(&format!("outside the workspace, {}", dir.display())),
            "{outside}"
        );
        fs::write(dir.join("b.bin"), [0u8, 1, 2]).unwrap();
        assert!(files.read("b.bin", None, None).unwrap_err().contains("binary"));
    }

    #[test]
    fn shows_a_file_before_overwriting_it() {
        let dir = workspace("write");
        fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let mut files = Files::new(&dir);
        let unread = files.write("a.py", "x = 2\n").unwrap_err();
        assert!(unread.contains("have not read") && unread.contains("     1\tx = 1\n"), "{unread}");
        assert_eq!(fs::read_to_string(dir.join("a.py")).unwrap(), "x = 1\n");
        files.write("a.py", "x = 2\n").unwrap();

        fs::write(dir.join("a.py"), "x = 3\n").unwrap();
        let changed = files.write("a.py", "x = 4\n").unwrap_err();
        assert!(changed.contains("changed since") && changed.contains("\tx = 3\n"), "{changed}");
        files.write("a.py", "x = 4\n").unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.py")).unwrap(), "x = 4\n");

        fs::write(dir.join("b.bin"), [0u8, 1, 2]).unwrap();
        assert!(files.write("b.bin", "text").unwrap_err().contains("binary file (3 bytes)"));
        files.write("new/c.py", "y = 1\n").unwrap();
        assert_eq!(fs::read_to_string(dir.join("new/c.py")).unwrap(), "y = 1\n");
    }

    #[test]
    fn edits_without_a_read() {
        let dir = workspace("unread");
        fs::write(dir.join("a.py"), "x = 1\ny = 2\n").unwrap();
        fs::write(dir.join("b.py"), "x = 1\n").unwrap();
        let mut files = Files::new(&dir);
        files.edit("a.py", "x = 1", "x = 5", false).unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.py")).unwrap(), "x = 5\ny = 2\n");
        // The edit showed only its own lines, so replacing the whole file still shows it first.
        assert!(files.write("a.py", "z = 3\n").unwrap_err().contains("have not read"));

        // A file read before stays known through an edit.
        files.read("b.py", None, None).unwrap();
        files.edit("b.py", "x = 1", "x = 2", false).unwrap();
        files.write("b.py", "x = 3\n").unwrap();
    }

    #[test]
    fn edits_exactly_once_and_shows_the_result() {
        let dir = workspace("edit");
        fs::write(dir.join("m.py"), "def f():\n    return 1\n\ndef g():\n    return 1\n").unwrap();
        let mut files = Files::new(&dir);
        files.read("m.py", None, None).unwrap();

        let twice = files.edit("m.py", "    return 1", "    return 2", false).unwrap_err();
        assert!(twice.contains("occurs 2 times") && twice.contains("lines 2, 5"), "{twice}");

        let out =
            files.edit("m.py", "def g():\n    return 1", "def g():\n    return 3", false).unwrap();
        assert!(out.contains("     5\t    return 3"), "{out}");
        assert_eq!(
            fs::read_to_string(dir.join("m.py")).unwrap(),
            "def f():\n    return 1\n\ndef g():\n    return 3\n"
        );

        files.edit("m.py", "return", "yield", true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("m.py")).unwrap(),
            "def f():\n    yield 1\n\ndef g():\n    yield 3\n"
        );
    }

    #[test]
    fn tolerates_newline_padding_and_crlf_and_explains_misses() {
        let dir = workspace("tolerant");
        fs::write(dir.join("a.py"), "a = 1\nb = 2").unwrap();
        fs::write(dir.join("w.txt"), "one\r\ntwo\r\n").unwrap();
        let mut files = Files::new(&dir);
        files.read("a.py", None, None).unwrap();
        files.read("w.txt", None, None).unwrap();

        files.edit("a.py", "\nb = 2\n", "\nb = 3\n", false).unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.py")).unwrap(), "a = 1\nb = 3");

        files.edit("w.txt", "one\ntwo", "uno\ndos", false).unwrap();
        assert_eq!(fs::read_to_string(dir.join("w.txt")).unwrap(), "uno\r\ndos\r\n");

        let miss = files.edit("a.py", "  b = 3\n  c = 4", "x", false).unwrap_err();
        assert!(miss.contains("at line 2"), "{miss}");
        let gone = files.edit("a.py", "zzz", "x", false).unwrap_err();
        assert!(gone.contains("neither was its first line"), "{gone}");
    }
}
