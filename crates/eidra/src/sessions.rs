//! `eidra sessions`: the sessions there are to go on with, with `--continue` or `--resume`
//! (docs/DESIGN.md §14).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use clap::Args;
use eidra_core::display::clip;
use eidra_core::{Summary, list_sessions};

use crate::exec::default_session_dir;

#[derive(Debug, Args)]
pub struct SessionsArgs {
    /// List the sessions of this directory [default: the current directory]
    #[arg(short = 'C', long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// List every directory's sessions
    #[arg(long, conflicts_with = "cwd")]
    all: bool,

    /// How many to list, the most recently active first
    #[arg(short = 'n', long, default_value_t = 20)]
    limit: usize,

    /// Where session logs are [default: ~/.eidra/sessions]
    #[arg(long, env = "EIDRA_SESSION_DIR", value_name = "DIR")]
    session_dir: Option<PathBuf>,
}

pub fn run(args: SessionsArgs) -> Result<()> {
    let here = match &args.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("the current directory")?,
    };
    let here = here.canonicalize().with_context(|| format!("{}", here.display()))?;
    let root = args.session_dir.clone().unwrap_or_else(|| default_session_dir(&here));
    // What was said in an encrypted session shows once its password is given.
    let vault = crate::vault::open(&eidra_core::eidra_home(&here), false)?;
    let sessions: Vec<Summary> = list_sessions(&root, vault.as_deref())
        .into_iter()
        .filter(|s| args.all || Path::new(&s.header.workspace) == here)
        .collect();
    if sessions.is_empty() {
        let whose = if args.all { String::new() } else { format!(" for {}", here.display()) };
        println!("no sessions{whose} in {}", root.display());
        return Ok(());
    }
    let now = SystemTime::now();
    for session in sessions.iter().take(args.limit) {
        let ago = now.duration_since(session.modified).map(|d| d.as_secs()).unwrap_or(0);
        let mut line = format!("{}  {:<10}", session.header.id, ago_text(ago));
        if args.all {
            line.push_str(&format!("  {}", session.header.workspace));
        }
        if session.header.headless {
            line.push_str("  (exec)");
        }
        let first = match (&session.first, session.sealed) {
            (Some(first), _) => first.as_str(),
            (None, true) => "(encrypted)",
            (None, false) => "",
        };
        let first = first.split_whitespace();
        line.push_str(&format!("  {}", clip(&first.collect::<Vec<_>>().join(" "), 70)));
        println!("{}", line.trim_end());
    }
    if sessions.len() > args.limit {
        println!("… and {} more (-n shows more)", sessions.len() - args.limit);
    }
    println!(
        "\n`eidra --resume ID` goes on with one, `eidra --continue` with the latest; `eidra exec` \
         takes the same for an (exec) session."
    );
    Ok(())
}

/// `3 min ago`, `5 h ago`, `2 days ago`.
fn ago_text(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_long_ago() {
        assert_eq!(ago_text(5), "just now");
        assert_eq!(ago_text(125), "2 min ago");
        assert_eq!(ago_text(7_300), "2 h ago");
        assert_eq!(ago_text(90_000), "yesterday");
        assert_eq!(ago_text(400_000), "4 days ago");
    }
}
