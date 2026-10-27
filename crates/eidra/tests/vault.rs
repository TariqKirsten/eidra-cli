//! Snapshots in an encrypted eidra directory, with real git and this binary as git's filter
//! (docs/DESIGN.md §18): nothing in the repository reads as the project's files or the user's
//! messages, and /diff, a resumed session and /undo still get them back whole.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use eidra_core::Header;
use eidra_core::checkpoint::Checkpoints;
use eidra_core::erase;
use eidra_core::vault::Vault;

fn git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// Every object in a repository as git keeps it.
fn objects(repository: &std::path::Path) -> String {
    let objects = Command::new("git")
        .arg("--git-dir")
        .arg(repository)
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .output()
        .unwrap();
    assert!(objects.status.success());
    String::from_utf8_lossy(&objects.stdout).into_owned()
}

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

    // Every object in the repository: file names, but no contents or labels.
    let repository = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    let objects = objects(&repository);
    for secret in ["swordfish", "rotated", "print('new')", "make the token"] {
        assert!(!objects.contains(secret), "{secret} is in the repository");
    }
    assert!(objects.contains("secret.py"), "the names of files stay readable");

    // /diff opens what it compares, and counts its lines as a text file's.
    let (turn, diff) = session.diff(false).await.unwrap().unwrap();
    assert_eq!(turn, 1);
    assert!(diff.contains("-API_TOKEN = 'swordfish'"), "{diff}");
    assert!(diff.contains("+API_TOKEN = 'rotated'"), "{diff}");
    assert!(diff.contains(" 2 files changed, 2 insertions(+), 1 deletion(-)"), "{diff}");
    assert!(!diff.contains("Bin "), "{diff}");

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

#[tokio::test]
async fn a_projects_line_ending_rules_do_not_touch_sealed_snapshots() {
    if !git() {
        return eprintln!("no git: skipped");
    }
    let dir = std::env::temp_dir().join(format!("eidra-sealed-eol-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let work = dir.join("work");
    fs::create_dir_all(&work).unwrap();
    // Rules that convert text on the way in and out, which git would apply to sealed bytes.
    let rules = "*.bat text eol=crlf\n*.py text eol=lf\n*.txt ident\n";
    fs::write(work.join(".gitattributes"), rules).unwrap();
    // Long enough that their sealed bytes hold line feeds, and carriage return and line feed
    // pairs, for those rules to change; sealing is the same each time, so this always is.
    let bat: Vec<u8> = (0..6000).flat_map(|i| format!("echo {i}\r\n").into_bytes()).collect();
    let py: Vec<u8> = (0..60000)
        .flat_map(|i| format!("v{i} = {i}{}", if i % 3 == 0 { "\r\n" } else { "\n" }).into_bytes())
        .collect();
    let txt = b"$Id$\n".to_vec();
    fs::write(work.join("run.bat"), &bat).unwrap();
    fs::write(work.join("mixed.py"), &py).unwrap();
    fs::write(work.join("id.txt"), &txt).unwrap();
    let vault = Arc::new(Vault::with_key([8; 32]));
    let program = PathBuf::from(env!("CARGO_BIN_EXE_eidra"));
    let mut session =
        Checkpoints::new(&dir.join("checkpoints"), &work, "s").unwrap().sealed(&program, vault);
    session.begin("change them").await.unwrap();
    for name in ["run.bat", "mixed.py", "id.txt"] {
        fs::write(work.join(name), "changed\n").unwrap();
    }
    session.end().await.unwrap();
    session.undo().await.unwrap().unwrap();
    // Back as they were, byte for byte.
    assert_eq!(fs::read(work.join("run.bat")).unwrap(), bat);
    assert_eq!(fs::read(work.join("mixed.py")).unwrap(), py);
    assert_eq!(fs::read(work.join("id.txt")).unwrap(), txt);
    let _ = fs::remove_dir_all(&dir);
}

/// A plain session's log, as one written before the vault: the header line alone.
fn plain_log(sessions: &std::path::Path, id: &str, workspace: &std::path::Path) {
    let header = Header {
        version: 1,
        id: id.into(),
        created: "2026-10-01T00:00:00Z".into(),
        workspace: workspace.display().to_string(),
        headless: false,
        profile: "p".into(),
        profile_hash: "h".into(),
        base_url: "http://x/v1".into(),
        model: "m".into(),
        reasoning_effort: "xhigh".into(),
        tools: vec![],
        parent: None,
        task: None,
    };
    let mut line = serde_json::to_value(&header).unwrap();
    line["type"] = "session".into();
    fs::create_dir_all(sessions).unwrap();
    fs::write(sessions.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
}

#[tokio::test]
async fn erasing_prunes_plain_snapshots_and_an_encrypted_session_still_undoes() {
    if !git() {
        return eprintln!("no git: skipped");
    }
    let dir = std::env::temp_dir().join(format!("eidra-erase-prune-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("work")).unwrap();
    let work = dir.join("work").canonicalize().unwrap();
    let (sessions, root) = (dir.join("sessions"), dir.join("checkpoints"));

    // A plain session from before the vault, with a snapshot of a secret.
    fs::write(work.join("app.py"), "TOKEN = 'plain-secret'\n").unwrap();
    plain_log(&sessions, "plain", &work);
    let mut plain = Checkpoints::new(&root, &work, "plain").unwrap();
    plain.begin("an old turn").await.unwrap();
    plain.end().await.unwrap();
    drop(plain);

    // An encrypted session, closed after a /diff that left an object only its index holds: the
    // file as it was then, which it still is, so its next snapshot takes it from the index.
    let vault = Arc::new(Vault::with_key([7; 32]));
    let program = PathBuf::from(env!("CARGO_BIN_EXE_eidra"));
    let sealed =
        || Checkpoints::new(&root, &work, "sealed").unwrap().sealed(&program, vault.clone());
    fs::write(work.join("app.py"), "TOKEN = 'rotated'\n").unwrap();
    let mut session = sealed();
    session.begin("rotate it").await.unwrap();
    fs::write(work.join("app.py"), "TOKEN = 'rotated twice'\n").unwrap();
    session.end().await.unwrap();
    fs::write(work.join("app.py"), "TOKEN = 'after the turn'\n").unwrap();
    // Older than the index the /diff writes, so git trusts that index's entry for it later
    // instead of reading the file again.
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
    fs::File::options().write(true).open(work.join("app.py")).unwrap().set_modified(past).unwrap();
    session.diff(false).await.unwrap();
    drop(session);

    let found = erase::find(&sessions, &root, "current");
    assert_eq!(found.sessions.len(), 1);
    let erased = erase::erase(&found, &sessions, &root).await.unwrap();
    assert_eq!((erased.sessions, erased.snapshots, erased.pending), (1, 1, 0));
    let repository = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    assert!(!objects(&repository).contains("plain-secret"), "the plain snapshot was pruned");

    // The encrypted session goes on, and its /diff and /undo still work. The file changed after
    // the turn, so the undo leaves it.
    let mut session = sealed().resumed().await.unwrap();
    assert!(session.diff(false).await.unwrap().is_some());
    let undone = session.undo().await.unwrap().unwrap();
    assert_eq!(undone.kept, ["app.py"]);
    assert_eq!(fs::read_to_string(work.join("app.py")).unwrap(), "TOKEN = 'after the turn'\n");
    let _ = fs::remove_dir_all(&dir);
}
