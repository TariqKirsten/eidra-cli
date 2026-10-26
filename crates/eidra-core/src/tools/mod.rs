//! The tools the model calls (docs/DESIGN.md §5). Parameters are flat strings, integers and
//! booleans: Qwen3.8 writes tool calls as XML, where those are natural and nested values are not.

mod bash;
mod diagnostics;
mod files;
mod search;

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use eidra_llm::{Tool, ToolCall};
use serde_json::{Map, Value, json};

use crate::confirm::ConfirmPolicy;
use crate::event::{Event, Frontend};
use crate::profile::Profile;
use crate::text::truncate_middle;

/// What a tool call produced.
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    /// Set by `finish`: the model's summary.
    pub finish: Option<String>,
}

pub struct Toolbox {
    workspace: PathBuf,
    definitions: Vec<Tool>,
    output_limit: usize,
    outputs: PathBuf,
    saved: usize,
    files: files::Files,
    shell: bash::Shell,
    diagnostics: diagnostics::Diagnostics,
}

enum Reply {
    Text(String),
    Finish(String),
}

/// The definitions of the profile's tools: in an interactive session with its interactive ones
/// too, and in a headless one with `finish`. A subagent gets the profile's subagent tools, and
/// never `task`: subagents do not start subagents.
pub fn definitions(profile: &Profile, headless: bool, subagent: bool) -> Result<Vec<Tool>> {
    let mut names = match (&profile.subagents.tools, subagent) {
        (Some(tools), true) => tools.clone(),
        _ => profile.tools.enabled.clone(),
    };
    if subagent {
        names.retain(|name| name != "task");
    }
    let interactive = !headless && !subagent;
    if interactive {
        for name in &profile.tools.interactive {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    if headless && !names.iter().any(|n| n == "finish") {
        names.push("finish".into());
    }
    names
        .iter()
        .map(|name| definition(name, profile.tools.descriptions.get(name), interactive))
        .collect()
}

impl Toolbox {
    /// Tools for `definitions`; `session_dir` holds saved output and the shell's state.
    pub fn new(
        profile: &Profile,
        workspace: &Path,
        session_dir: &Path,
        definitions: Vec<Tool>,
    ) -> Result<Self> {
        Ok(Toolbox {
            workspace: workspace.to_path_buf(),
            definitions,
            output_limit: profile.tools.output_limit,
            outputs: session_dir.join("outputs"),
            saved: 0,
            files: files::Files::new(workspace),
            diagnostics: diagnostics::Diagnostics::new(workspace, profile.diagnostics.clone()),
            shell: bash::Shell::new(
                workspace,
                &session_dir.join("shell"),
                profile.api_key_env.as_deref(),
            )?,
        })
    }

    pub async fn call<F: Frontend>(
        &mut self,
        call: &ToolCall,
        confirm: &mut ConfirmPolicy,
        frontend: &mut F,
    ) -> ToolOutput {
        let name = call.function.name.as_str();
        let result = if self.definitions.iter().any(|d| d.function.name == name) {
            match arguments(&call.function.arguments) {
                Ok(args) => self.dispatch(name, &args, confirm, frontend).await,
                Err(e) => Err(e),
            }
        } else {
            let names: Vec<&str> =
                self.definitions.iter().map(|d| d.function.name.as_str()).collect();
            Err(format!("there is no tool called {name}; the tools are {}", names.join(", ")))
        };
        match result {
            Ok(Reply::Text(text)) => {
                ToolOutput { text: self.limit(name, text), is_error: false, finish: None }
            }
            Ok(Reply::Finish(summary)) => {
                ToolOutput { text: "Finished.".into(), is_error: false, finish: Some(summary) }
            }
            Err(error) => ToolOutput {
                text: self.limit(name, format!("Error: {error}")),
                is_error: true,
                finish: None,
            },
        }
    }

    async fn dispatch<F: Frontend>(
        &mut self,
        name: &str,
        args: &Value,
        confirm: &mut ConfirmPolicy,
        frontend: &mut F,
    ) -> Result<Reply, String> {
        let text = match name {
            "bash" => {
                let command = string(args, "command")?;
                let timeout = number(args, "timeout_s")?;
                let background = boolean(args, "background")?.unwrap_or(false);
                self.shell.run(command, timeout, background, confirm, frontend).await?
            }
            "bash_output" => self.shell.job_output(required_number(args, "id")?)?,
            "bash_kill" => self.shell.kill_job(required_number(args, "id")?).await?,
            "read" => self.files.read(
                string(args, "path")?,
                number(args, "offset")?,
                number(args, "limit")?,
            )?,
            "write" | "edit" => {
                // The file's problems before the change, so that the result names only new ones.
                let path = string(args, "path")?;
                let before = self.diagnostics.before(&resolve(&self.workspace, path)).await;
                let mut text = if name == "write" {
                    self.files.write(path, string(args, "content")?)?
                } else {
                    self.files.edit(
                        path,
                        string(args, "old")?,
                        string(args, "new")?,
                        boolean(args, "replace_all")?.unwrap_or(false),
                    )?
                };
                if let Some(before) = before {
                    let problems = self.diagnostics.after(before).await;
                    if !problems.is_empty() {
                        text.truncate(text.trim_end().len());
                        text.push_str(&problems);
                    }
                }
                text
            }
            "glob" => search::glob(
                &self.workspace,
                string(args, "pattern")?,
                optional_string(args, "path")?,
            )?,
            "grep" => search::grep(
                &self.workspace,
                string(args, "pattern")?,
                optional_string(args, "path")?,
                optional_string(args, "glob")?,
            )?,
            "todo" => {
                frontend.event(Event::Todo { list: string(args, "list")?.to_owned() });
                "The todo list is updated.".into()
            }
            "finish" => return Ok(Reply::Finish(string(args, "summary")?.to_owned())),
            // The agent runs `task` itself, since it starts another agent.
            _ => return Err(format!("{name} is not implemented")),
        };
        Ok(Reply::Text(text))
    }

    /// Output past the profile's limit is cut in the middle, and saved whole for the model to read.
    fn limit(&mut self, name: &str, text: String) -> String {
        if text.len() <= self.output_limit {
            return text;
        }
        self.saved += 1;
        let path = self.outputs.join(format!("{:03}-{name}.txt", self.saved));
        let note = match fs::create_dir_all(&self.outputs).and_then(|()| fs::write(&path, &text)) {
            Ok(()) => format!("; the whole output is in {}", path.display()),
            Err(_) => String::new(),
        };
        truncate_middle(&text, self.output_limit, &note)
    }
}

/// `path` resolved against the workspace, with `.` and `..` taken out.
pub(crate) fn resolve(workspace: &Path, path: &str) -> PathBuf {
    let joined = workspace.join(path.trim());
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// What a `task` call asks for.
pub(crate) struct TaskCall {
    pub description: String,
    pub prompt: String,
    /// The call returns at once, and the subagent reports later.
    pub background: bool,
}

pub(crate) fn task_arguments(call: &ToolCall) -> Result<TaskCall, String> {
    let args = arguments(&call.function.arguments)?;
    Ok(TaskCall {
        description: string(&args, "description")?.trim().to_owned(),
        prompt: string(&args, "prompt")?.to_owned(),
        background: boolean(&args, "background")?.unwrap_or(false),
    })
}

fn arguments(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_str(raw) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(format!("the arguments must be a JSON object, not {raw}")),
    }
}

fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    optional_string(args, key)?.ok_or_else(|| format!("missing the {key} parameter"))
}

fn optional_string<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(format!("{key} must be a string, not {other}")),
    }
}

/// A non-negative integer, given as a number or as digits in a string.
fn number(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            n.as_u64().map(Some).ok_or_else(|| format!("{key} must be a whole number, not {n}"))
        }
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| format!("{key} must be a whole number, not {s:?}")),
        Some(other) => Err(format!("{key} must be a whole number, not {other}")),
    }
}

fn required_number(args: &Value, key: &str) -> Result<u64, String> {
    number(args, key)?.ok_or_else(|| format!("missing the {key} parameter"))
}

fn boolean(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::String(s)) => match s.trim() {
            "true" => Ok(Some(true)),
            "false" | "" => Ok(Some(false)),
            _ => Err(format!("{key} must be true or false, not {s:?}")),
        },
        Some(other) => Err(format!("{key} must be true or false, not {other}")),
    }
}

/// A tool's definition, with the profile's description if it gives one. In an interactive session,
/// `task` can run in the background.
fn definition(name: &str, description: Option<&String>, interactive: bool) -> Result<Tool> {
    let (default, properties, required): (&str, Value, &[&str]) = match name {
        "bash" => (
            "Run a bash command and return its output (stdout and stderr together) and exit code. The \
             working directory and exported variables carry over to the next command. Commands run \
             without a terminal, so nothing can prompt for input. Run servers, watchers and other \
             long-running processes with background set to true.",
            json!({
                "command": {"type": "string", "description": "The command to run."},
                "timeout_s": {"type": "integer", "description": "Seconds before the command is stopped: 120 if unset, at most 600."},
                "background": {"type": "boolean", "description": "Start the command in the background and return at once with a job id."}
            }),
            &["command"],
        ),
        "bash_output" => (
            "Return the output a background job has produced since you last asked, and whether it is still running.",
            json!({"id": {"type": "integer", "description": "The job id bash returned."}}),
            &["id"],
        ),
        "bash_kill" => (
            "Stop a background job and everything it started.",
            json!({"id": {"type": "integer", "description": "The job id bash returned."}}),
            &["id"],
        ),
        "read" => (
            "Read a text file. Lines come back numbered; read a long file in parts with offset and \
             limit.",
            json!({
                "path": {"type": "string", "description": "The file, relative to the working directory or absolute."},
                "offset": {"type": "integer", "description": "The line to start at, counting from 1."},
                "limit": {"type": "integer", "description": "How many lines to return: 2000 if unset."}
            }),
            &["path"],
        ),
        "edit" => (
            "Replace text in a file. old must match the file exactly, indentation included, and occur \
             only once unless replace_all is true; include a few surrounding lines to make it unique.",
            json!({
                "path": {"type": "string", "description": "The file to change."},
                "old": {"type": "string", "description": "The exact text to replace."},
                "new": {"type": "string", "description": "The text to put in its place."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence of old."}
            }),
            &["path", "old", "new"],
        ),
        "write" => (
            "Create a file, or replace a whole file, with the given content. To change part of a file, \
             use edit. Read an existing file with read before you replace it.",
            json!({
                "path": {"type": "string", "description": "The file to write. Missing directories are created."},
                "content": {"type": "string", "description": "The complete new content of the file."}
            }),
            &["path", "content"],
        ),
        "glob" => (
            "Find files by name, such as *.py or src/**/test_*.py. A pattern without a slash matches file \
             names in every directory. Hidden files and files ignored by .gitignore are skipped.",
            json!({
                "pattern": {"type": "string", "description": "The glob pattern."},
                "path": {"type": "string", "description": "The directory to search: the working directory if unset."}
            }),
            &["pattern"],
        ),
        "grep" => (
            "Search file contents with a regular expression (Rust regex syntax) and return the matching \
             lines as path:line:text. Hidden files and files ignored by .gitignore are skipped.",
            json!({
                "pattern": {"type": "string", "description": "The regular expression."},
                "path": {"type": "string", "description": "A file or directory to search: the working directory if unset."},
                "glob": {"type": "string", "description": "Only search files whose names match this glob, such as *.py."}
            }),
            &["pattern"],
        ),
        "todo" => (
            "Keep a checklist for work with several steps. Send the whole list each time, as markdown \
             checkboxes: - [ ] for a step to do, - [x] for a step done.",
            json!({"list": {"type": "string", "description": "The whole checklist."}}),
            &["list"],
        ),
        "task" => (
            "Hand a self-contained piece of work to a subagent: a new agent with the same tools, which \
             starts knowing nothing of this conversation and returns only its final summary. Give it \
             everything it needs: the goal, the files and facts that matter, and what to report back. \
             Several task calls in one reply run at the same time; give each its own files to change. \
             Good uses: searching many files when only the answer matters, independent changes you \
             can describe completely, reviewing a change.",
            {
                let mut properties = json!({
                    "description": {"type": "string", "description": "A few words naming the task."},
                    "prompt": {"type": "string", "description": "The complete instructions for the subagent."}
                });
                if interactive {
                    properties["background"] = json!({"type": "boolean", "description": "Start the subagent in the background and return at once, so that you can go on with the user while it works. Its report comes later, as a message of its own. For work that takes a while; leave the files it works on alone until it reports."});
                }
                properties
            },
            &["description", "prompt"],
        ),
        "finish" => (
            "End the task. Call it only when the task is complete and checked, with a short summary of \
             what you did and how you checked it.",
            json!({"summary": {"type": "string", "description": "What you did and how you checked it."}}),
            &["summary"],
        ),
        _ => bail!("the profile enables a tool eidra does not have: {name}"),
    };
    let parameters = json!({"type": "object", "properties": properties, "required": required});
    Ok(Tool::function(name, description.map_or(default, String::as_str), parameters))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_paths_against_the_workspace() {
        let ws = Path::new("/work");
        assert_eq!(resolve(ws, "a/./b.py"), Path::new("/work/a/b.py"));
        assert_eq!(resolve(ws, "../etc/x"), Path::new("/etc/x"));
        assert_eq!(resolve(ws, "/tmp/y"), Path::new("/tmp/y"));
        assert_eq!(resolve(ws, "."), Path::new("/work"));
    }

    #[test]
    fn reads_arguments_leniently_but_checks_their_types() {
        let args = arguments(r#"{"n": 5, "s": "7", "b": "true", "x": [1]}"#).unwrap();
        assert_eq!(number(&args, "n").unwrap(), Some(5));
        assert_eq!(number(&args, "s").unwrap(), Some(7));
        assert_eq!(boolean(&args, "b").unwrap(), Some(true));
        assert_eq!(number(&args, "missing").unwrap(), None);
        assert!(string(&args, "x").is_err());
        assert!(arguments("[1, 2]").is_err());
        assert!(arguments("").unwrap().as_object().unwrap().is_empty());
    }

    #[test]
    fn every_builtin_tool_has_a_definition() {
        let profile = Profile::builtin().unwrap();
        for name in profile.tools.enabled.iter().map(String::as_str).chain(["task", "finish"]) {
            let tool = definition(name, None, false).unwrap();
            assert_eq!(tool.function.name, name);
            assert_eq!(tool.function.parameters["type"], "object");
        }
        assert!(definition("teleport", None, false).is_err());
        let custom = definition("read", Some(&"Read it.".to_owned()), false).unwrap();
        assert_eq!(custom.function.description, "Read it.");
    }

    #[test]
    fn interactive_sessions_add_their_tools_and_a_background_task() {
        let profile = Profile::builtin().unwrap();
        let task = |tools: &[Tool]| tools.iter().find(|t| t.function.name == "task").cloned();
        let interactive = definitions(&profile, false, false).unwrap();
        let background =
            &task(&interactive).unwrap().function.parameters["properties"]["background"];
        assert_eq!(background["type"], "boolean");
        // Unattended runs, the evals among them, keep the tool list they had.
        let headless = definitions(&profile, true, false).unwrap();
        assert!(task(&headless).is_none());
        let names: Vec<&str> = headless.iter().map(|t| t.function.name.as_str()).collect();
        let mut expected: Vec<&str> = profile.tools.enabled.iter().map(String::as_str).collect();
        expected.push("finish");
        assert_eq!(names, expected);
    }

    #[test]
    fn subagents_never_get_task() {
        let names = |tools: Vec<Tool>| -> Vec<String> {
            tools.into_iter().map(|t| t.function.name).collect()
        };
        let mut profile = Profile::builtin().unwrap();
        profile.tools.enabled = vec!["read".into(), "task".into()];
        profile.tools.interactive.clear();
        assert_eq!(names(definitions(&profile, false, false).unwrap()), ["read", "task"]);
        assert_eq!(names(definitions(&profile, true, true).unwrap()), ["read", "finish"]);
        profile.subagents.tools = Some(vec!["grep".into(), "task".into()]);
        assert_eq!(names(definitions(&profile, true, true).unwrap()), ["grep", "finish"]);
    }
}
