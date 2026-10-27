//! eidra's own directory, `~/.eidra`: the user's AGENTS.md, the session logs and the checkpoints,
//! in one place to find, back up or mount (docs/DESIGN.md §14).

use std::path::{Path, PathBuf};

/// `$EIDRA_HOME`, else `~/.eidra`, else `.eidra` in the workspace (a container user without a
/// home).
pub fn eidra_home(workspace: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("EIDRA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() && home != "/" => PathBuf::from(home).join(".eidra"),
        _ => workspace.join(".eidra"),
    }
}
