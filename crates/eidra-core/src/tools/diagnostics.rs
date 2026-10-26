//! Problems a change introduced (docs/DESIGN.md §9). Before and after the agent edits or writes a
//! file, the profile's checks for its language run on it, and the tool's result names only what
//! is new: a problem that was there before the change is the project's, not the agent's.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::profile::DiagnosticsSettings;
use crate::text::fnv1a;

/// Problems shown at most for one change.
const MOST: usize = 20;
/// ty rules left out: whether an import resolves depends on ty finding the project's environment,
/// which it often cannot, and a false alarm sends the model after a problem that is not there.
const TY_IGNORED: &[&str] = &["unresolved-import"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem {
    line: usize,
    /// What it is, without where: before and after are compared on this, since lines move.
    what: String,
    source: &'static str,
}

/// A file's problems before a change.
pub struct Before {
    path: PathBuf,
    problems: Vec<Problem>,
}

pub struct Diagnostics {
    workspace: PathBuf,
    settings: DiagnosticsSettings,
    /// Where tools are looked for, in order.
    search: Vec<PathBuf>,
    /// Problems by file and content, so that the result after one change is the start of the next.
    cache: HashMap<(PathBuf, u64), Vec<Problem>>,
}

impl Diagnostics {
    /// Tools are looked for in `$EIDRA_TOOLS`, the workspace's `.venv/bin`, then `PATH`.
    pub fn new(workspace: &Path, settings: DiagnosticsSettings) -> Self {
        let mut search: Vec<PathBuf> = Vec::new();
        for var in ["EIDRA_TOOLS", "PATH"] {
            if var == "PATH" {
                search.push(workspace.join(".venv/bin"));
            }
            if let Some(dirs) = std::env::var_os(var) {
                search.extend(std::env::split_paths(&dirs));
            }
        }
        Self::with_search(workspace, settings, search)
    }

    fn with_search(workspace: &Path, settings: DiagnosticsSettings, search: Vec<PathBuf>) -> Self {
        let workspace = workspace.to_path_buf();
        Diagnostics { workspace, settings, search, cache: HashMap::new() }
    }

    /// The file's problems as it is now, before a change; None if no checks apply to it.
    pub async fn before(&mut self, path: &Path) -> Option<Before> {
        self.checks(path)?;
        let problems = match fs::read_to_string(path) {
            Ok(content) => self.problems(path, &content).await,
            // A new file: whatever it has, the change brought.
            Err(_) => Vec::new(),
        };
        Some(Before { path: path.to_path_buf(), problems })
    }

    /// What the change introduced, as text to add to the tool's result; empty if nothing.
    pub async fn after(&mut self, before: Before) -> String {
        let Ok(content) = fs::read_to_string(&before.path) else {
            return String::new();
        };
        let after = self.problems(&before.path, &content).await;
        let new = introduced(&before.problems, &after);
        if new.is_empty() {
            return String::new();
        }
        let shown = before.path.strip_prefix(&self.workspace).unwrap_or(&before.path);
        let mut text = format!("\n\nThis change introduced problems in {}:", shown.display());
        for problem in new.iter().take(MOST) {
            text.push_str(&format!(
                "\n  line {}: {} ({})",
                problem.line, problem.what, problem.source
            ));
        }
        if new.len() > MOST {
            text.push_str(&format!("\n  … and {} more", new.len() - MOST));
        }
        text
    }

    /// The checks for a file's language, if it has any.
    fn checks(&self, path: &Path) -> Option<&[String]> {
        let extension = path.extension()?.to_str()?;
        let checks = match extension {
            "py" | "pyi" => &self.settings.python,
            _ => return None,
        };
        (!checks.is_empty()).then_some(checks.as_slice())
    }

    /// Every check's problems with `path` holding `content`.
    async fn problems(&mut self, path: &Path, content: &str) -> Vec<Problem> {
        let key = (path.to_path_buf(), fnv1a(content.as_bytes()));
        if let Some(problems) = self.cache.get(&key) {
            return problems.clone();
        }
        let checks = self.checks(path).map(<[String]>::to_vec).unwrap_or_default();
        let timeout = Duration::from_secs(self.settings.timeout_s.max(1));
        let mut problems = Vec::new();
        let mut parsed = false;
        if checks.iter().any(|c| c == "ruff")
            && let Some(ruff) = self.tool("ruff")
            && let Some(found) = ruff_check(&ruff, &self.workspace, path, content, timeout).await
        {
            problems.extend(found);
            // ruff reports syntax errors too.
            parsed = true;
        }
        if checks.iter().any(|c| c == "ty")
            && let Some(ty) = self.tool("ty")
            && let Some(found) = ty_check(&ty, &self.workspace, path, timeout).await
        {
            // An undefined name is ruff's F821 and ty's unresolved-reference: say it once.
            let undefined: Vec<usize> = problems
                .iter()
                .filter(|p| p.source == "ruff" && p.what.ends_with("[F821]"))
                .map(|p| p.line)
                .collect();
            problems.extend(found.into_iter().filter(|p| {
                !(p.what.ends_with("[unresolved-reference]") && undefined.contains(&p.line))
            }));
        }
        if checks.iter().any(|c| c == "syntax") && !parsed {
            problems.extend(python_syntax(content));
        }
        self.cache.insert(key, problems.clone());
        problems
    }

    fn tool(&self, name: &str) -> Option<PathBuf> {
        self.search.iter().map(|dir| dir.join(name)).find(|path| {
            fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    }
}

/// The problems in `after` that `before` did not have, counting repeats.
fn introduced(before: &[Problem], after: &[Problem]) -> Vec<Problem> {
    let mut left: HashMap<(&str, &str), usize> = HashMap::new();
    for problem in before {
        *left.entry((problem.source, problem.what.as_str())).or_default() += 1;
    }
    let mut new: Vec<Problem> = after
        .iter()
        .filter(|problem| match left.get_mut(&(problem.source, problem.what.as_str())) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .cloned()
        .collect();
    new.sort_by_key(|problem| problem.line);
    new
}

/// Runs a command with a time limit; None if it could not start or ran out of time.
async fn output(mut command: Command, stdin: Option<&str>, timeout: Duration) -> Option<Vec<u8>> {
    command
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let text = text.to_owned();
        tokio::spawn(async move {
            let _ = pipe.write_all(text.as_bytes()).await;
        });
    }
    let done = tokio::time::timeout(timeout, child.wait_with_output()).await.ok()?.ok()?;
    Some(done.stdout)
}

/// `ruff check` on the content, as the project configures it.
async fn ruff_check(
    ruff: &Path,
    workspace: &Path,
    path: &Path,
    content: &str,
    timeout: Duration,
) -> Option<Vec<Problem>> {
    let mut command = Command::new(ruff);
    command.args(["check", "--output-format", "json", "--no-fix", "--no-cache"]);
    command.arg("--stdin-filename");
    command.arg(path).arg("-").current_dir(workspace);
    let stdout = output(command, Some(content), timeout).await?;
    let items: Vec<Value> = serde_json::from_slice(&stdout).ok()?;
    Some(
        items
            .iter()
            .map(|item| {
                let message = item["message"].as_str().unwrap_or("a problem");
                let what = match item["code"].as_str() {
                    Some(code) => format!("{message} [{code}]"),
                    None => message.to_owned(),
                };
                let line = item["location"]["row"].as_u64().unwrap_or(0) as usize;
                Problem { line, what, source: "ruff" }
            })
            .collect(),
    )
}

/// `ty check` on the file as it is on disk, with the project's `.venv` if it has one.
async fn ty_check(
    ty: &Path,
    workspace: &Path,
    path: &Path,
    timeout: Duration,
) -> Option<Vec<Problem>> {
    let mut command = Command::new(ty);
    command.args(["check", "--output-format", "concise"]).current_dir(workspace);
    let venv = workspace.join(".venv");
    if venv.is_dir() {
        command.arg("--python").arg(venv);
    }
    command.arg(path);
    let stdout = output(command, None, timeout).await?;
    Some(String::from_utf8_lossy(&stdout).lines().filter_map(ty_line).collect())
}

/// One line of ty's concise output: `file:line:column: severity[rule] message`.
fn ty_line(line: &str) -> Option<Problem> {
    let mut parts = line.splitn(4, ':');
    let (_file, row, _column) = (parts.next()?, parts.next()?, parts.next()?);
    let row: usize = row.trim().parse().ok()?;
    let rest = parts.next()?.trim();
    let open = rest.find('[')?;
    let close = rest[open..].find(']')? + open;
    let rule = &rest[open + 1..close];
    if TY_IGNORED.contains(&rule) {
        return None;
    }
    let message = rest[close + 1..].trim();
    Some(Problem { line: row, what: format!("{message} [{rule}]"), source: "ty" })
}

/// Syntax errors, from a parse with tree-sitter's Python grammar: a check that needs no tool.
fn python_syntax(content: &str) -> Vec<Problem> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&tree_sitter_python::LANGUAGE.into()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if !node.has_error() {
            continue;
        }
        if node.is_error() || node.is_missing() {
            let what = if node.is_missing() {
                format!("syntax error: missing {}", node.kind())
            } else {
                "syntax error".to_owned()
            };
            problems.push(Problem { line: node.start_position().row + 1, what, source: "parse" });
            continue;
        }
        stack.extend(node.children(&mut cursor));
    }
    problems.sort_by_key(|problem| problem.line);
    problems.truncate(3);
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(line: usize, what: &str, source: &'static str) -> Problem {
        Problem { line, what: what.into(), source }
    }

    #[test]
    fn only_problems_the_change_brought_count() {
        let before = [problem(1, "`os` imported but unused [F401]", "ruff")];
        let after = [
            problem(3, "`os` imported but unused [F401]", "ruff"),
            problem(9, "Undefined name `pathlib` [F821]", "ruff"),
            problem(2, "Undefined name `pathlib` [F821]", "ruff"),
        ];
        let new = introduced(&before, &after);
        assert_eq!(new.iter().map(|p| p.line).collect::<Vec<_>>(), [2, 9]);
    }

    #[test]
    fn reads_ty_lines_and_leaves_out_import_resolution() {
        let problem = ty_line(
            "m.py:5:16: error[unresolved-reference] Name `undefined_name` used when not defined",
        )
        .unwrap();
        assert_eq!(problem.line, 5);
        assert_eq!(
            problem.what,
            "Name `undefined_name` used when not defined [unresolved-reference]"
        );
        assert!(
            ty_line("m.py:1:8: error[unresolved-import] Cannot resolve import `numpy`").is_none()
        );
        assert!(ty_line("Found 1 diagnostic").is_none());
    }

    #[test]
    fn finds_syntax_errors_without_a_tool() {
        assert!(python_syntax("def f(x):\n    return x + 1\n").is_empty());
        let problems = python_syntax("def f(x):\n    return (x + 1\n\nprint(f(2))\n");
        assert!(!problems.is_empty());
        assert!(problems[0].what.starts_with("syntax error"), "{problems:?}");
    }

    /// A workspace, and a directory of fake tools that report what a real one would.
    fn setup(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("eidra-diag-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (work, tools) = (dir.join("work"), dir.join("tools"));
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&tools).unwrap();
        (work, tools)
    }

    fn fake_tool(dir: &Path, name: &str, script: &str) {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn settings(checks: &[&str]) -> DiagnosticsSettings {
        let python = checks.iter().map(|c| (*c).to_owned()).collect();
        DiagnosticsSettings { python, timeout_s: 5 }
    }

    #[tokio::test]
    async fn reports_what_ruff_finds_new_after_a_change() {
        let (work, tools) = setup("ruff");
        // Reports an undefined name for every line of stdin that uses `missing`.
        fake_tool(
            &tools,
            "ruff",
            r#"n=0; echo '['; sep=''; while IFS= read -r line; do n=$((n+1)); case "$line" in *missing*) printf '%s{"code":"F821","message":"Undefined name `missing`","location":{"row":%d,"column":1}}' "$sep" "$n"; sep=',';; esac; done; echo ']'"#,
        );
        let file = work.join("app.py");
        fs::write(&file, "x = missing\n").unwrap();
        let mut diagnostics =
            Diagnostics::with_search(&work, settings(&["ruff", "syntax"]), vec![tools]);
        let before = diagnostics.before(&file).await.unwrap();
        fs::write(&file, "x = missing\ny = 2\nz = missing\n").unwrap();
        let text = diagnostics.after(before).await;
        assert_eq!(
            text,
            "\n\nThis change introduced problems in app.py:\n  line 3: Undefined name `missing` [F821] (ruff)"
        );
        let _ = fs::remove_dir_all(work.parent().unwrap());
    }

    #[tokio::test]
    async fn without_ruff_a_parse_finds_syntax_errors() {
        let (work, _) = setup("parse");
        let file = work.join("app.py");
        let mut diagnostics =
            Diagnostics::with_search(&work, settings(&["ruff", "syntax"]), vec![]);
        let before = diagnostics.before(&file).await.unwrap();
        fs::write(&file, "def f(:\n    pass\n").unwrap();
        let text = diagnostics.after(before).await;
        assert!(
            text.contains("introduced problems in app.py") && text.contains("syntax error"),
            "{text}"
        );
        // Other languages have no checks.
        assert!(diagnostics.before(&work.join("notes.md")).await.is_none());
        let _ = fs::remove_dir_all(work.parent().unwrap());
    }

    #[tokio::test]
    async fn a_check_that_takes_too_long_is_left_out() {
        let (work, tools) = setup("slow");
        fake_tool(&tools, "ty", "sleep 5; echo 'app.py:1:1: error[x] never seen'");
        let file = work.join("app.py");
        fs::write(&file, "x = 1\n").unwrap();
        let mut slow = settings(&["ty"]);
        slow.timeout_s = 1;
        let mut diagnostics = Diagnostics::with_search(&work, slow, vec![tools]);
        let before = diagnostics.before(&file).await.unwrap();
        fs::write(&file, "x = 2\n").unwrap();
        assert_eq!(diagnostics.after(before).await, "");
        let _ = fs::remove_dir_all(work.parent().unwrap());
    }
}
