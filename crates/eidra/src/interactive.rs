//! `eidra` without a command: an interactive session in the terminal (docs/DESIGN.md §12).

use std::io::IsTerminal;

use anyhow::{Result, bail};

use crate::exec::AgentArgs;

pub async fn run(args: AgentArgs) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("an interactive session needs a terminal; `eidra exec` runs a task unattended");
    }
    let workspace = args.workspace()?;
    eidra_tui::run(eidra_tui::Options {
        profile: args.profile()?,
        session_root: args.session_root(&workspace),
        resume: args.session_to_resume(&workspace, false)?,
        checkpoints: Some(eidra_core::eidra_home(&workspace).join("checkpoints")),
        workspace,
        confirm: args.confirm.clone(),
    })
    .await
}
