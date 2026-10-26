//! Eval tasks. A task is a directory holding:
//! - task.toml
//! - workspace/: what the agent starts with
//! - check/: mounted at /check only while the checks run, never shown to the agent
//! - solution/: a reference solution laid over workspace/, used only by `eidra eval validate`
//!
//! A directory with an instruction.md is a Harbor-format task instead (harbor.rs). See
//! evals/README.md.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::harbor;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub id: String,
    pub split: Split,
    /// Where the task comes from, when it is not original.
    #[serde(default)]
    #[allow(dead_code)] // For people reading the task; nothing reads it.
    pub origin: Option<String>,
    /// The container image that the agent and the checks run in.
    pub image: String,
    pub prompt: String,
    #[serde(default = "default_max_requests")]
    pub max_requests: usize,
    /// Wall-clock limit for the agent.
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    pub checks: Vec<Check>,
}

/// Train tasks are for tuning; test tasks are held out, to catch tuning that only fits the train set.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Train,
    Test,
}

impl Split {
    pub fn name(self) -> &'static str {
        match self {
            Split::Train => "train",
            Split::Test => "test",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    /// A shell command run in a copy of the finished workspace; the check passes if it exits 0.
    pub run: String,
    #[serde(default = "default_check_timeout")]
    pub timeout_s: u64,
}

fn default_max_requests() -> usize {
    100
}

fn default_timeout() -> u64 {
    1200
}

fn default_check_timeout() -> u64 {
    300
}

pub struct Task {
    pub spec: Spec,
    pub dir: PathBuf,
    /// Set for a Harbor-format task, whose spec is made from its instruction.md and task.toml.
    pub harbor: Option<harbor::Config>,
}

impl Task {
    pub fn id(&self) -> &str {
        &self.spec.id
    }

    pub fn workspace(&self) -> PathBuf {
        self.dir.join("workspace")
    }

    pub fn check_dir(&self) -> Option<PathBuf> {
        let dir = self.dir.join("check");
        dir.is_dir().then_some(dir)
    }

    pub fn solution(&self) -> PathBuf {
        self.dir.join("solution")
    }
}

/// Every task in `suite`, in id order.
pub fn load_suite(suite: &Path) -> Result<Vec<Task>> {
    let entries =
        fs::read_dir(suite).with_context(|| format!("reading the suite {}", suite.display()))?;
    let mut tasks = Vec::new();
    for entry in entries {
        let dir = entry?.path();
        let file = dir.join("task.toml");
        if !file.is_file() {
            continue;
        }
        if dir.join("instruction.md").is_file() {
            tasks.push(load_harbor(&dir)?);
            continue;
        }
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let spec: Spec = toml::from_str(&text).with_context(|| format!("{}", file.display()))?;
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if spec.id != name {
            bail!("{}: the id {:?} is not the directory's name", file.display(), spec.id);
        }
        if !dir.join("workspace").is_dir() {
            bail!("{}: there is no workspace/ directory", dir.display());
        }
        if spec.checks.is_empty() {
            bail!("{}: a task needs at least one check", file.display());
        }
        tasks.push(Task { spec, dir, harbor: None });
    }
    if tasks.is_empty() {
        bail!("there are no tasks in {}", suite.display());
    }
    tasks.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    Ok(tasks)
}

/// A Harbor-format task. Harbor has no splits, and outside tasks are not tuned on, so they count
/// as test tasks.
fn load_harbor(dir: &Path) -> Result<Task> {
    let config = harbor::Config::load(dir)?;
    let prompt = fs::read_to_string(dir.join("instruction.md"))
        .with_context(|| format!("reading {}", dir.join("instruction.md").display()))?;
    let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
    let spec = Spec {
        id,
        split: Split::Test,
        origin: config.source.clone(),
        image: config.docker_image.clone().unwrap_or_default(),
        prompt: prompt.trim().to_owned(),
        max_requests: harbor::MAX_REQUESTS,
        timeout_s: config.agent_timeout_s.ceil() as u64,
        checks: Vec::new(),
    };
    Ok(Task { spec, dir: dir.to_path_buf(), harbor: Some(config) })
}
