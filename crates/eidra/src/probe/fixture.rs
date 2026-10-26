//! A small generated Python project for the probe to read, and the tools that read it.

use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use eidra_llm::{Tool, ToolCall};
use serde_json::{Value, json};

/// The modules the task has the model read: name, docstring, and the field their code reads.
pub const MODULES: [(&str, &str, &str); 5] = [
    ("parse", "Reads expense rows from CSV text into typed records.", "category"),
    ("stats", "Totals and averages over expense records.", "amount"),
    ("budget", "Compares spending per category against monthly budgets.", "budget"),
    ("fmt", "Formats reports as aligned plain-text tables.", "label"),
    ("cli", "The command-line entry point.", "option"),
];

pub const TASK: &str = "List src/ledger/, then read each of its modules, one file per turn, \
    with read_file. When you have read all five, call finish with a one-line description of each \
    module.";

/// A temporary project directory, removed on drop.
pub struct Fixture {
    root: PathBuf,
}

/// The result of one tool call.
pub struct Outcome {
    pub output: String,
    pub finished: bool,
}

impl Fixture {
    pub fn create() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("eidra-probe-{}", std::process::id()));
        let package = root.join("src/ledger");
        fs::create_dir_all(&package).with_context(|| format!("creating {}", package.display()))?;
        let fixture = Fixture { root };
        fixture.write(
            "README.md",
            "# ledger\n\nA small expense-report tool: parse a CSV export, total it, check \
             budgets, print a report.\n",
        )?;
        fixture.write(
            "pyproject.toml",
            "[project]\nname = \"ledger\"\nversion = \"0.1.0\"\nrequires-python = \">=3.11\"\n",
        )?;
        fixture.write("src/ledger/__init__.py", "\"\"\"Expense reports.\"\"\"\n")?;
        for (name, summary, field) in MODULES {
            fixture
                .write(&format!("src/ledger/{name}.py"), &module_source(name, summary, field))?;
        }
        Ok(fixture)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn write(&self, relative: &str, text: &str) -> Result<()> {
        let path = self.root.join(relative);
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// The tools on offer, with flat string parameters (docs/DESIGN.md §5).
    pub fn tools() -> Vec<Tool> {
        let path = |what: &str| {
            json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": what}},
                "required": ["path"]
            })
        };
        vec![
            Tool::function(
                "list_dir",
                "List the files and directories at a path in the project.",
                path("Directory relative to the project root, such as \".\" or \"src/ledger\"."),
            ),
            Tool::function(
                "read_file",
                "Read a text file in the project. Lines come back numbered.",
                path("File relative to the project root."),
            ),
            Tool::function(
                "finish",
                "End the task with your final answer.",
                json!({
                    "type": "object",
                    "properties": {"summary": {"type": "string", "description": "Your final answer."}},
                    "required": ["summary"]
                }),
            ),
        ]
    }

    /// Runs one call. Errors become its output, as a tool reports them to the model.
    pub fn run(&self, call: &ToolCall) -> Outcome {
        let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
        let arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let result = match call.function.name.as_str() {
            "list_dir" => self.list_dir(&arg("path")),
            "read_file" => self.read_file(&arg("path")),
            "finish" => return Outcome { output: "Done.".into(), finished: true },
            other => Err(format!("there is no tool called {other}")),
        };
        Outcome { output: result.unwrap_or_else(|e| format!("error: {e}")), finished: false }
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let relative = Path::new(path.trim());
        if relative.is_absolute() || relative.components().any(|c| c == Component::ParentDir) {
            return Err(format!("{path}: only paths inside the project can be used"));
        }
        Ok(self.root.join(relative))
    }

    fn list_dir(&self, path: &str) -> Result<String, String> {
        let entries = fs::read_dir(self.resolve(path)?).map_err(|e| format!("{path}: {e}"))?;
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() { name + "/" } else { name }
            })
            .collect();
        names.sort();
        Ok(names.join("\n"))
    }

    fn read_file(&self, path: &str) -> Result<String, String> {
        let text = fs::read_to_string(self.resolve(path)?).map_err(|e| format!("{path}: {e}"))?;
        let mut out = String::with_capacity(text.len() + text.len() / 4);
        for (n, line) in text.lines().enumerate() {
            let _ = writeln!(out, "{:>6}\t{line}", n + 1);
        }
        Ok(out)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// About 200 lines of plausible Python: twenty small functions over expense rows.
fn module_source(stem: &str, summary: &str, field: &str) -> String {
    let mut s = format!(
        "\"\"\"{summary}\"\"\"\n\nfrom __future__ import annotations\n\nfrom typing import Iterable\n"
    );
    for i in 0..20 {
        let limit = 8 + i;
        let _ = write!(
            s,
            "\n\ndef {stem}_rule_{i}(rows: Iterable[dict[str, str]], limit: int = {limit}) -> list[str]:\n    \
             \"\"\"Return the `{field}` values longer than `limit` characters, cut to `limit` (rule {i}).\"\"\"\n    \
             out: list[str] = []\n    \
             for row in rows:\n        \
             value = row.get(\"{field}\", \"\").strip()\n        \
             if len(value) > limit:\n            \
             out.append(value[:limit])\n    \
             return out\n"
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidra_llm::FunctionCall;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_0".into(),
            kind: "function".into(),
            function: FunctionCall { name: name.into(), arguments: arguments.into() },
        }
    }

    #[test]
    fn reads_lists_and_refuses_to_leave_the_project() {
        let fixture = Fixture::create().unwrap();
        let listing = fixture.run(&call("list_dir", r#"{"path": "src/ledger"}"#)).output;
        assert_eq!(listing, "__init__.py\nbudget.py\ncli.py\nfmt.py\nparse.py\nstats.py");

        let source = fixture.run(&call("read_file", r#"{"path": "src/ledger/stats.py"}"#)).output;
        assert!(source.starts_with("     1\t\"\"\"Totals and averages"));
        assert!(source.contains("def stats_rule_19("));

        let escape = fixture.run(&call("read_file", r#"{"path": "../../etc/passwd"}"#)).output;
        assert!(escape.starts_with("error: "));

        let done = fixture.run(&call("finish", r#"{"summary": "x"}"#));
        assert!(done.finished);
    }
}
