//! #89: a failed hook's evidence. `newgit action --quiet` keeps a long
//! command's output in its log and says how it went; a failure, there and
//! in spawn's `prepare: FAILED`, shows the command that ran and how the log
//! ends.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

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
    let dir = std::env::temp_dir().join(format!("newgit-quiet-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path, resource: &str) -> Utf8PathBuf {
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
    std::fs::write(store.paths().resources.join("db.toml"), resource).expect("write resource");
    repo
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

/// A thousand lines of noise, then the line that says why, then exit 3.
const NOISY: &str = r#"ownership = "branch"

[actions.prepare]
command = "true"

[actions.migrate]
command = "seq 1000; echo 'migration 7 failed: relation users exists' >&2; exit 3"
"#;

#[test]
fn quiet_action_keeps_the_output_in_the_log_and_shows_its_end() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp, NOISY);
    let (code, _, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stderr}");

    let (code, stdout, stderr) = newgit(&repo, &["action", "db.migrate", "x", "--quiet"]);
    assert_eq!(code, Some(3), "{stdout}{stderr}");
    assert!(
        !stdout.contains("\n500\n"),
        "the output reached the terminal:\n{stdout}"
    );
    assert!(
        stderr.contains("[newgit] ran: seq 1000; echo 'migration 7 failed"),
        "{stderr}"
    );
    assert!(
        stderr.contains("  | migration 7 failed: relation users exists"),
        "{stderr}"
    );
    assert!(stderr.contains("  | 1000"), "{stderr}");
    assert!(!stderr.contains("  | 900\n"), "only the end: {stderr}");
    let log = stderr
        .lines()
        .find_map(|line| line.split("; log: ").nth(1))
        .expect("log path");
    let text = std::fs::read_to_string(log.trim()).expect("log");
    assert!(text.contains("\n500\n"), "the log has all of it");

    // Loud by default, as before.
    let (code, stdout, _) = newgit(&repo, &["action", "db.migrate", "x"]);
    assert_eq!(code, Some(3));
    assert!(stdout.contains("\n500\n"), "{stdout}");
}

#[test]
fn quiet_success_prints_only_the_exit() {
    let (_guard, temp) = tempdir();
    let repo = setup(
        &temp,
        "ownership = \"branch\"\n\n[actions.seed]\ncommand = \"seq 50\"\n",
    );
    newgit(&repo, &["spawn", "x"]);
    let (code, stdout, stderr) = newgit(&repo, &["action", "db.seed", "x", "-q"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(!stdout.contains("25"), "{stdout}");
    assert!(stderr.contains("`db.seed` exit 0"), "{stderr}");
    assert!(!stderr.contains("ran:"), "{stderr}");
}

#[test]
fn a_failed_prepare_at_spawn_shows_the_command_it_ran() {
    let (_guard, temp) = tempdir();
    let repo = setup(
        &temp,
        "ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"pg_dump_not_installed --schema-only\"\n",
    );
    let (code, stdout, _) = newgit(&repo, &["spawn", "x"]);
    assert_ne!(code, Some(0));
    assert!(
        stdout.contains("ran: pg_dump_not_installed --schema-only"),
        "{stdout}"
    );
    assert!(stdout.contains("not found"), "{stdout}");
    assert!(
        !stdout.contains("| [newgit] $"),
        "the header is said once, as `ran:`: {stdout}"
    );
}
