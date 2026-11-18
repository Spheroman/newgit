//! `undo` waits for what it restarts and exits by whether the instance is
//! up, as `spawn` and `start` do (#276), and so does `spawn
//! --from-checkpoint`, which finishes with an undo.
//!
//! Found setting up Remix + Prisma + Postgres: `undo` restarted the
//! database and returned at once, so SQL run straight after it got
//! "connection refused", and its exit code said nothing about readiness.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::Value;

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
    let dir = std::env::temp_dir().join(format!(
        "newgit-undo-waits-for-ready-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
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
    store
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A database that takes a second to come up after each start: up once the
/// process that is running now has written its pid to `pid`, so a pid left
/// by the run the undo stopped does not count. Never up while `broken`
/// exists.
fn slow_database(store: &MetadataStore, temp: &Utf8Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let pid = temp.join("db.pid");
    let broken = temp.join("broken");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!(
            "ownership = \"branch\"\n\n[actions.serve]\n\
             command = \"sleep 1; echo $$ > {pid}; exec sleep 120\"\n\
             long_running = true\nautostart = true\n\n[ready]\n\
             command = \"test ! -f {broken} && kill -0 $(cat {pid})\"\ntimeout_seconds = 3\n"
        ),
    )
    .expect("write resource");
    (pid, broken)
}

/// What the database's check asks, asked by the test right after newgit
/// returns.
fn up(pid: &Utf8Path) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 $(cat {pid})"))
        .status()
        .is_ok_and(|status| status.success())
}

struct Stops<'a>(&'a Utf8Path, &'a [&'a str]);

impl Drop for Stops<'_> {
    fn drop(&mut self) {
        for instance in self.1 {
            let _ = newgit(self.0, &["stop", instance]);
        }
    }
}

#[test]
fn undo_waits_for_what_it_restarted_and_says_ready() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let (pid, _) = slow_database(&store, &temp);
    let repo = store.paths().project_root.clone();
    let _stops = Stops(&repo, &["x"]);

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    let (code, stdout, stderr) = newgit(&repo, &["undo", "x"]);
    assert!(up(&pid), "the database is up when undo returns:\n{stdout}");
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("  resource: db none + restarted ok\n"),
        "{stdout}"
    );
    assert!(stdout.ends_with("ready: `x`\n"), "{stdout}");
}

#[test]
fn undo_exits_1_when_what_it_restarted_is_not_up() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let (_, broken) = slow_database(&store, &temp);
    let repo = store.paths().project_root.clone();
    let _stops = Stops(&repo, &["x"]);

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    std::fs::write(&broken, "").expect("broken");
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x"]);
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    // The undo itself did all it set out to; the instance is not up.
    assert!(stdout.contains("Restored `x` to ckpt_001"), "{stdout}");
    assert!(stdout.contains("not ready: `x`"), "{stdout}");
    assert!(stdout.contains("`db` is not up after 3s"), "{stdout}");
}

#[test]
fn spawn_from_a_checkpoint_waits_and_reports_as_spawn_does() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let (pid, broken) = slow_database(&store, &temp);
    let repo = store.paths().project_root.clone();
    let _stops = Stops(&repo, &["x", "y", "z"]);

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["stop", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    let (code, stdout, stderr) = newgit(
        &repo,
        &["spawn", "y", "--from-checkpoint", "x/ckpt_001", "--json"],
    );
    assert!(up(&pid), "the database is up when spawn returns:\n{stdout}");
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let json: Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(json["starting"], serde_json::json!([]), "{json}");
    assert_eq!(json["problems"], serde_json::json!([]), "{json}");
    let (code, stdout, stderr) = newgit(&repo, &["stop", "y"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    std::fs::write(&broken, "").expect("broken");
    let (code, stdout, stderr) = newgit(&repo, &["spawn", "z", "--from-checkpoint", "x/ckpt_001"]);
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    assert!(stdout.contains("not ready: `z`"), "{stdout}");
    assert!(stdout.contains("`db` is not up after 3s"), "{stdout}");
}
