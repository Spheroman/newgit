//! A process an undo could not start again is down, not failed, and the
//! next start that succeeds is what clears it.
//!
//! Found setting up Remix + Prisma + Postgres: an undo whose restart of
//! `web` failed marked `web`'s binding failed. The retried undo restarted it
//! and printed `restarted ok, left failed`; `status` kept calling the
//! instance not ready while it served 200, and suggested `newgit action
//! web.start`, which is refused because it is running. `stop` then `start`
//! did not clear it either, since no start ever touched the binding. Only a
//! rebind did, and that moved a port.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

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
    let dir = std::env::temp_dir().join(format!(
        "newgit-undo-restart-failure-{}-{n}",
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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
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

/// A database that is up unless `down` exists, and a server that waits for
/// it: while `down` exists, the undo's restart of `web` fails.
fn database_and_server(store: &MetadataStore, down: &Utf8Path) {
    write_resource(
        store,
        "db",
        &format!(
            "ownership = \"branch\"\n\n[actions.serve]\ncommand = \"exec sleep 120\"\n\
             long_running = true\nautostart = true\n\n[ready]\ncommand = \"test ! -f {down}\"\n\
             timeout_seconds = 1\n"
        ),
    );
    write_resource(
        store,
        "web",
        "ownership = \"branch\"\ndepends_on = [\"db\"]\n\n[actions.serve]\n\
         command = \"exec sleep 120\"\nlong_running = true\nautostart = true\n",
    );
}

/// Stops what the test started, whatever it asserted.
struct Stops<'a>(&'a Utf8Path);

impl Drop for Stops<'_> {
    fn drop(&mut self) {
        let _ = newgit(self.0, &["stop", "x"]);
    }
}

#[test]
fn a_failed_restart_reads_not_started_and_start_clears_it() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let down = temp.join("db-down");
    database_and_server(&store, &down);
    let repo = store.paths().project_root.clone();
    let _stops = Stops(&repo);

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    std::fs::write(&down, "").expect("down");
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x"]);
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    assert!(
        stdout.contains("  resource: web none ok, restart FAILED\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("`web` restored, but not running: the restart failed"),
        "not a failed restore command:\n{stdout}"
    );
    assert!(!stdout.contains("partial state"), "{stdout}");

    // Down, and how to bring it back — not `failed`, which no start clears.
    let (_, status, _) = newgit(&repo, &["status", "x"]);
    assert!(
        status.contains("`web.serve` was not started — start it with `newgit start`"),
        "{status}"
    );
    assert!(!status.contains("web:failed"), "{status}");

    std::fs::remove_file(&down).expect("up");
    let (code, stdout, stderr) = newgit(&repo, &["start", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("Started `web.serve`"), "{stdout}");
    assert!(stdout.contains("ready: `x`"), "{stdout}");
    let (_, status, _) = newgit(&repo, &["status", "x"]);
    assert!(status.contains("web:running"), "{status}");
    assert!(!status.contains("not ready"), "{status}");
}

#[test]
fn a_retried_undo_that_restarts_leaves_nothing_failed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let down = temp.join("db-down");
    database_and_server(&store, &down);
    let repo = store.paths().project_root.clone();
    let _stops = Stops(&repo);

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    std::fs::write(&down, "").expect("down");
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x", "--to", "ckpt_001"]);
    assert_eq!(code, Some(1), "{stdout}{stderr}");

    std::fs::remove_file(&down).expect("up");
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x", "--to", "ckpt_001"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("  resource: web none + restarted ok\n"),
        "{stdout}"
    );
    // Its capture was not skipped as "known-broken" on the way, either.
    assert!(!stderr.contains("in a failed state"), "{stderr}");
    let (_, status, _) = newgit(&repo, &["status", "x"]);
    assert!(status.contains("web:running"), "{status}");
    assert!(!status.contains("not ready"), "{status}");

    // Nor after a stop and a start.
    let (code, stdout, stderr) = newgit(&repo, &["stop", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["start", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("ready: `x`"), "{stdout}");
}

/// The failure a `prepare` records is cleared by the `prepare` succeeding,
/// by hand as at spawn.
#[test]
fn a_prepare_that_succeeds_by_hand_clears_its_failure() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let flag = temp.join("can-prepare");
    write_resource(
        &store,
        "deps",
        &format!("ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"test -f {flag}\"\n"),
    );
    let repo = store.paths().project_root.clone();

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    let (_, status, _) = newgit(&repo, &["status", "x"]);
    assert!(status.contains("deps:failed"), "{status}");

    std::fs::write(&flag, "").expect("flag");
    let (code, stdout, stderr) = newgit(&repo, &["action", "deps.prepare", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (_, status, _) = newgit(&repo, &["status", "x"]);
    assert!(status.contains("deps:ready"), "{status}");
    assert!(!status.contains("not ready"), "{status}");
}
