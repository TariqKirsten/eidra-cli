//! `/erase-unsecure` (docs/DESIGN.md §18). Once eidra's directory is encrypted, the sessions from
//! before are the only unencrypted ones left in it: this finds them and erases them, with their
//! saved files, their subagents' logs and their snapshots. A session open in another eidra is
//! left alone. Erased files are gone from the filesystem, but on an SSD or a copy-on-write
//! filesystem their blocks may stay on the disk until written over.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::checkpoint;
use crate::session::{self, in_use};

/// The unencrypted sessions found.
#[derive(Debug, Default)]
pub struct Unencrypted {
    /// Sessions to erase, subagents' among them.
    pub sessions: Vec<Found>,
    /// Unencrypted sessions open in another eidra, left alone.
    pub open: usize,
    /// What they take on the disk, their directories included.
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Found {
    pub id: String,
    /// The directory it worked in.
    pub workspace: String,
    pub subagent: bool,
    path: PathBuf,
}

/// What an erase did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Erased {
    pub sessions: usize,
    /// Sessions whose snapshots went.
    pub snapshots: usize,
}

/// The unencrypted sessions in `root`, other than `current`.
pub fn find(root: &Path, current: &str) -> Unencrypted {
    let mut found = Unencrypted::default();
    for summary in session::all(root, None) {
        if summary.sealed || summary.header.id == current {
            continue;
        }
        if in_use(&summary.path) {
            found.open += 1;
            continue;
        }
        found.bytes += size(&summary.path) + size(&summary.path.with_extension(""));
        found.sessions.push(Found {
            id: summary.header.id,
            workspace: summary.header.workspace,
            subagent: summary.header.parent.is_some(),
            path: summary.path,
        });
    }
    found.sessions.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// Erases what `find` found, which must have been found in `root`, and their snapshots in the
/// repositories under `checkpoints`.
pub async fn erase(found: &Unencrypted, root: &Path, checkpoints: &Path) -> Result<Erased, String> {
    let mut erased = Erased::default();
    let mut ids = HashSet::new();
    for session in &found.sessions {
        // Opened since it was found: left alone.
        if in_use(&session.path) {
            continue;
        }
        fs::remove_file(&session.path)
            .or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
            .map_err(|e| format!("removing {}: {e}", session.path.display()))?;
        let dir = session.path.with_extension("");
        if dir.is_dir() {
            fs::remove_dir_all(&dir).map_err(|e| format!("removing {}: {e}", dir.display()))?;
        }
        ids.insert(session.id.clone());
        erased.sessions += 1;
    }
    // A repository an open session works in may be taking a snapshot: git prunes it later.
    let busy: HashSet<String> = session::all(root, None)
        .iter()
        .filter(|summary| in_use(&summary.path))
        .map(|summary| checkpoint::repository_name(Path::new(&summary.header.workspace)))
        .collect();
    erased.snapshots = checkpoint::forget_sessions(checkpoints, &ids, &busy).await?;
    Ok(erased)
}

/// Bytes a file or a directory takes, what is in it included.
fn size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .map(|entries| entries.flatten().map(|entry| size(&entry.path())).sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use eidra_llm::Message;

    use super::*;
    use crate::session::{Header, Session};
    use crate::vault::Vault;

    fn header(parent: Option<&str>) -> Header {
        Header {
            version: 1,
            id: String::new(),
            created: String::new(),
            workspace: "/nowhere/work".into(),
            headless: parent.is_some(),
            profile: "p".into(),
            profile_hash: "h".into(),
            base_url: "http://x/v1".into(),
            model: "m".into(),
            reasoning_effort: "medium".into(),
            tools: vec![],
            parent: parent.map(str::to_owned),
            task: parent.map(|_| "a task".to_owned()),
        }
    }

    #[tokio::test]
    async fn erases_the_unencrypted_sessions_but_the_open_and_the_encrypted() {
        let root = std::env::temp_dir().join(format!("eidra-erase-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sessions = root.join("sessions");
        let vault = Arc::new(Vault::with_key([5; 32]));
        let mut old = Session::create(&sessions, header(None), None).unwrap();
        old.push(Message::user("from before the vault"), None).unwrap();
        let old_id = old.header().id.clone();
        fs::create_dir_all(old.dir().join("outputs")).unwrap();
        fs::write(old.dir().join("outputs/001-bash.txt"), "output").unwrap();
        let sub = Session::create(&sessions, header(Some(&old_id)), None).unwrap();
        let (old_path, sub_path) = (old.path().to_path_buf(), sub.path().to_path_buf());
        drop((old, sub));
        // Open in another eidra: left alone.
        let open = Session::create(&sessions, header(None), None).unwrap();
        // Encrypted, and this one's own: neither is erased.
        let current = Session::create(&sessions, header(None), Some(vault.clone())).unwrap();
        let sealed = Session::create(&sessions, header(None), Some(vault)).unwrap();
        let sealed_path = sealed.path().to_path_buf();
        drop(sealed);

        let found = find(&sessions, &current.header().id);
        let ids: Vec<&str> = found.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(found.sessions.len(), 2, "{ids:?}");
        assert!(found.sessions.iter().any(|s| s.subagent));
        assert_eq!(found.open, 1);
        assert!(found.bytes > 0);

        let erased = erase(&found, &sessions, &root.join("checkpoints")).await.unwrap();
        assert_eq!(erased, Erased { sessions: 2, snapshots: 0 });
        assert!(!old_path.exists() && !old_path.with_extension("").exists());
        assert!(!sub_path.exists());
        assert!(open.path().exists() && current.path().exists() && sealed_path.exists());
        drop((open, current));
        let _ = fs::remove_dir_all(&root);
    }
}
