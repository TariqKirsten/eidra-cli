//! Snapshots in an encrypted eidra directory, with real git and this binary as git's filter
//! (docs/DESIGN.md §18): nothing in the repository reads as the project's files or the user's
//! messages, and /diff, a resumed session and /undo still get them back whole.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use eidra_core::checkpoint::Checkpoints;
use eidra_core::vault::Vault;

#[tokio::test]
async fn snapshots_are_sealed_and_come_back_whole() {
    if Command::new("git").arg("--version").output().is_err() {
        return eprintln!("no git: skipped");
    }
    let dir = std::env::temp_dir().join(format!("eidra-sealed-snapshots-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let work = dir.join("work");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("secret.py"), "API_TOKEN = 'swordfish'\n").unwrap();
    let vault = Arc::new(Vault::with_key([9; 32]));
    let program = PathBuf::from(env!("CARGO_BIN_EXE_eidra"));
    let root = dir.join("checkpoints");
    let checkpoints =
        || Checkpoints::new(&root, &work, "s1").unwrap().sealed(&program, vault.clone());

    let mut session = checkpoints();
    session.begin("make the token a secret please").await.unwrap();
    fs::write(work.join("secret.py"), "API_TOKEN = 'rotated'\n").unwrap();
    fs::write(work.join("new.py"), "print('new')\n").unwrap();
    session.end().await.unwrap();

    // Every object in the repository, as git keeps it: file names, but no contents or labels.
    let repository = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    let objects = Command::new("git")
        .arg("--git-dir")
        .arg(&repository)
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .output()
        .unwrap();
    assert!(objects.status.success());
    let objects = String::from_utf8_lossy(&objects.stdout);
    for secret in ["swordfish", "rotated", "print('new')", "make the token"] {
        assert!(!objects.contains(secret), "{secret} is in the repository");
    }
    assert!(objects.contains("secret.py"), "the names of files stay readable");

    // /diff opens what it compares.
    let (turn, diff) = session.diff(false).await.unwrap().unwrap();
    assert_eq!(turn, 1);
    assert!(diff.contains("-API_TOKEN = 'swordfish'"), "{diff}");
    assert!(diff.contains("+API_TOKEN = 'rotated'"), "{diff}");

    // A session that goes on finds its turn, and the label it began with.
    let resumed = checkpoints().resumed().await.unwrap();
    assert_eq!(resumed.turns()[0].label, "make the token a secret please");

    // /undo puts the file back as it was, in plain text, and removes the one the turn made.
    let undone = session.undo().await.unwrap().unwrap();
    assert_eq!(undone.label, "make the token a secret please");
    assert_eq!(fs::read_to_string(work.join("secret.py")).unwrap(), "API_TOKEN = 'swordfish'\n");
    assert!(!work.join("new.py").exists());
    let _ = fs::remove_dir_all(&dir);
}
