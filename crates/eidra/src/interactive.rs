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
        checkpoints: Some(crate::exec::data_dir(&workspace).join("checkpoints")),
        workspace,
        confirm: args.confirm.clone(),
    })
    .await
}
