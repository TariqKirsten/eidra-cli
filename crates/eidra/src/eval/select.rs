//! `eidra eval select`: a suite made of some of a downloaded Harbor dataset's tasks, as symlinks.
//!
//! SWE-rebench tasks describe themselves in tests/config.json (`language`, `created_at`), which is
//! what the filters read. A task without it passes only filters that are not set.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use eidra_core::utc_datetime;
use serde_json::Value;

#[derive(Debug, Args)]
pub struct SelectArgs {
    /// The downloaded dataset: a directory of Harbor task directories
    #[arg(long, value_name = "DIR")]
    from: PathBuf,

    /// The suite to make; selected tasks are linked into it
    #[arg(long, value_name = "DIR")]
    to: PathBuf,

    /// Only tasks in this language (SWE-rebench's `language`, such as python)
    #[arg(long)]
    language: Option<String>,

    /// Only tasks created on or after this date (YYYY-MM-DD)
    #[arg(long, value_name = "DATE")]
    since: Option<String>,

    /// Only tasks created before this date (YYYY-MM-DD)
    #[arg(long, value_name = "DATE")]
    until: Option<String>,

    /// Leave this task out; repeatable
    #[arg(long, value_name = "ID")]
    exclude: Vec<String>,

    /// At most this many tasks, the newest first
    #[arg(long)]
    limit: Option<usize>,
}

struct Candidate {
    id: String,
    dir: PathBuf,
    language: Option<String>,
    created: Option<String>,
}

pub fn run(args: &SelectArgs) -> Result<i32> {
    let from = fs::canonicalize(&args.from)
        .with_context(|| format!("the dataset {}", args.from.display()))?;
    let mut chosen = Vec::new();
    for entry in fs::read_dir(&from).with_context(|| format!("reading {}", from.display()))? {
        let dir = entry?.path();
        if !dir.join("instruction.md").is_file() || !dir.join("task.toml").is_file() {
            continue;
        }
        let candidate = describe(&dir);
        if args.exclude.contains(&candidate.id) {
            continue;
        }
        let language_ok = args.language.as_ref().is_none_or(|want| {
            candidate.language.as_ref().is_some_and(|l| l.eq_ignore_ascii_case(want))
        });
        let date = candidate.created.as_deref();
        let since_ok = args.since.as_deref().is_none_or(|s| date.is_some_and(|d| d >= s));
        let until_ok = args.until.as_deref().is_none_or(|u| date.is_some_and(|d| d < u));
        if language_ok && since_ok && until_ok {
            chosen.push(candidate);
        }
    }
    if chosen.is_empty() {
        bail!("no task in {} matches", from.display());
    }
    chosen.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
    if let Some(limit) = args.limit {
        chosen.truncate(limit);
    }
    fs::create_dir_all(&args.to).with_context(|| format!("creating {}", args.to.display()))?;
    for task in &chosen {
        let link = args.to.join(&task.id);
        if link.symlink_metadata().is_ok() {
            fs::remove_file(&link).with_context(|| format!("replacing {}", link.display()))?;
        }
        std::os::unix::fs::symlink(&task.dir, &link)
            .with_context(|| format!("linking {}", link.display()))?;
        println!(
            "{:<44} {:<10} {}",
            task.id,
            task.language.as_deref().unwrap_or("-"),
            task.created.as_deref().unwrap_or("-")
        );
    }
    println!("{} tasks linked into {}", chosen.len(), args.to.display());
    Ok(0)
}

fn describe(dir: &Path) -> Candidate {
    let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
    let config: Option<Value> = fs::read_to_string(dir.join("tests/config.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let field = |name: &str| config.as_ref().and_then(|c| c.get(name)).cloned();
    Candidate {
        id,
        dir: dir.to_path_buf(),
        language: field("language").and_then(|v| v.as_str().map(str::to_owned)),
        created: field("created_at").and_then(|v| date(&v)),
    }
}

/// A `created_at` as YYYY-MM-DD: milliseconds since the epoch, or a date-time string.
fn date(value: &Value) -> Option<String> {
    if let Some(ms) = value.as_f64() {
        let (y, m, d, ..) = utc_datetime((ms / 1000.0) as u64);
        return Some(format!("{y:04}-{m:02}-{d:02}"));
    }
    value.as_str().filter(|s| s.len() >= 10).map(|s| s[..10].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_dates_in_both_forms() {
        assert_eq!(date(&json!(1780001218000_u64)).as_deref(), Some("2026-05-28"));
        assert_eq!(date(&json!("2026-03-04T10:00:00Z")).as_deref(), Some("2026-03-04"));
        assert_eq!(date(&json!("soon")), None);
    }
}
