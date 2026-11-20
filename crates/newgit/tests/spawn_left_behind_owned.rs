//! Review of #83: the note a spawn prints about a secret it left behind
//! quotes a path a shell would split, and does not name a file a tracker
//! already owns.

#![allow(dead_code)]

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::{Value, json};

fn git(dir: &Utf8Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-leftowned-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

/// A project whose root has a committed `.gitignore` and, beside it, the
/// ignored files a developer's checkout accumulates.
fn setup(temp: &Utf8Path) -> Utf8PathBuf {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(
        repo.join(".gitignore"),
        ".env.local\nconfig/master.key\nnode_modules/\n*.log\n",
    )
    .expect("write");
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");

    std::fs::write(repo.join(".env.local"), "TOKEN=hunter2\n").expect("write");
    std::fs::create_dir_all(repo.join("config")).expect("mkdir");
    std::fs::write(repo.join("config/master.key"), "abc\n").expect("write");
    // Not secrets: an install and a log say nothing.
    std::fs::create_dir_all(repo.join("node_modules/left-pad")).expect("mkdir");
    std::fs::write(repo.join("node_modules/left-pad/index.js"), "\n").expect("write");
    std::fs::write(repo.join("debug.log"), "\n").expect("write");
    repo
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.success(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn newgit_ok(repo: &Utf8Path, args: &[&str]) -> String {
    let (ok, stdout, stderr) = newgit(repo, args);
    assert!(ok, "newgit {args:?} failed:\n{stdout}\n{stderr}");
    stdout
}

#[test]
fn a_path_with_a_space_is_quoted_in_the_command_it_prints() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    std::fs::write(repo.join("my creds.secret.json"), "{}\n").expect("write");
    std::fs::write(repo.join(".dev.vars"), "TOKEN=1\n").expect("write");
    let mut ignore = std::fs::read_to_string(repo.join(".gitignore")).expect(".gitignore");
    ignore.push_str("*.secret.json\n.dev.vars\n");
    std::fs::write(repo.join(".gitignore"), ignore).expect("write");

    let stdout = newgit_ok(&repo, &["spawn", "a"]);
    assert!(stdout.contains("left behind: .dev.vars"), "{stdout}");
    assert!(stdout.contains("'my creds.secret.json'"), "{stdout}");
}

#[test]
fn a_file_a_tracker_already_owns_is_not_named() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    // Owned, and nothing captured for it yet: the workspace lacks the file,
    // and making a tracker for it is not the answer.
    newgit_ok(
        &repo,
        &["tracker", "create", "local-env", "--audience", "user"],
    );
    newgit_ok(&repo, &["tracker", "track", "local-env", ".env.local"]);

    let spawned: Value =
        serde_json::from_str(&newgit_ok(&repo, &["spawn", "a", "--json"])).expect("json");
    assert_eq!(
        spawned["left_behind"],
        json!(["config/master.key"]),
        "{spawned}"
    );
}
