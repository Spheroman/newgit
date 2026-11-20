//! What is running when a `[restore]` runs, and what an undo whose restore
//! failed leaves.
//!
//! Found setting up Remix + Prisma + a per-instance Postgres: the undo
//! stops every process before restoring, so a restore that needs a server
//! meets a stopped one, and a failed restore left the server's dependent
//! (`web`) stopped with nothing saying why or what to run. `status` then
//! suggested starting the database's process, over the half-done restore.
//! And a dependent's restore ran while the dependency the undo had just
//! started again was still coming up.
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
        "newgit-undo-restore-order-{}-{n}",
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

/// A database newgit supervises that takes a second to come up after each
/// start: up once the process running now has written its pid. Its restore
/// writes whether its own server is up into `trace`, and fails while `fail`
/// exists.
fn database(store: &MetadataStore, temp: &Utf8Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let pid = temp.join("db.pid");
    let trace = temp.join("trace");
    let fail = temp.join("fail");
    let up = format!("kill -0 $(cat {pid}) 2>/dev/null");
    write_resource(
        store,
        "db",
        &format!(
            r#"ownership = "branch"

[actions.serve]
command = "sleep 1; echo $$ > {pid}; exec sleep 120"
long_running = true
autostart = true

[ready]
command = "{up}"
timeout_seconds = 5

[checkpoint]
mode = "command"
command = "echo dump"

[restore]
mode = "command"
command = "if {up}; then echo db-restore: up; else echo db-restore: down; fi >> {trace}; test ! -f {fail}"
"#
        ),
    );
    (trace, fail)
}

/// A seed that loads rows into the database: its restore needs it up.
fn seed(store: &MetadataStore, temp: &Utf8Path) {
    let pid = temp.join("db.pid");
    write_resource(
        store,
        "seed",
        &format!(
            r#"ownership = "branch"
depends_on = ["db"]

[checkpoint]
mode = "command"
command = "echo rows"

[restore]
mode = "command"
command = "kill -0 $(cat {pid})"
"#
        ),
    );
}

fn web(store: &MetadataStore) {
    write_resource(
        store,
        "web",
        "ownership = \"branch\"\ndepends_on = [\"db\"]\n\n[actions.serve]\n\
         command = \"exec sleep 120\"\nlong_running = true\nautostart = true\n",
    );
}

struct Stops<'a>(&'a Utf8Path);

impl Drop for Stops<'_> {
    fn drop(&mut self) {
        let _ = newgit(self.0, &["stop", "x"]);
    }
}

#[test]
fn a_restore_runs_with_its_own_process_stopped_and_its_dependencies_up() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let (trace, _) = database(&store, &temp);
    seed(&store, &temp);
    let repo = store.paths().project_root.clone();
    let _stops = Stops(&repo);

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    let (code, stdout, stderr) = newgit(&repo, &["undo", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    // Its own server is down for its restore, as the reference says.
    assert_eq!(
        std::fs::read_to_string(&trace).expect("trace"),
        "db-restore: down\n"
    );
    assert!(
        stdout.contains("  resource: db command + restarted ok\n"),
        "{stdout}"
    );
    // The seed's restore waited for the database the undo started again.
    assert!(stdout.contains("  resource: seed command ok\n"), "{stdout}");
    assert!(stdout.ends_with("ready: `x`\n"), "{stdout}");
}

#[test]
fn a_failed_restore_leaves_it_and_its_dependents_down_and_says_how_to_retry() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let (_, fail) = database(&store, &temp);
    web(&store);
    let repo = store.paths().project_root.clone();
    let _stops = Stops(&repo);

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    std::fs::write(&fail, "").expect("fail");
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x", "--to", "ckpt_001"]);
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    assert!(
        stdout.contains("  resource: db command FAILED\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  resource: web none ok, not restarted, left blocked by db\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("`web` restored, but not running (below)"),
        "{stdout}"
    );
    assert!(stdout.contains("not ready: `x`"), "{stdout}");

    // `status` names the retry that is safe: another undo, not starting the
    // database's process over a restore that did not finish.
    let (_, status, _) = newgit(&repo, &["status", "x"]);
    assert!(status.contains("db:failed"), "{status}");
    assert!(status.contains("web:blocked(db)"), "{status}");
    assert!(
        status.contains(
            "`db` failed to restore: restore command exited with 1 — repair it, then `newgit \
             undo` again"
        ),
        "{status}"
    );
    assert!(!status.contains("newgit action db.serve"), "{status}");

    // `start` does not bring either back over it.
    let (code, stdout, _) = newgit(&repo, &["start", "x"]);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(!stdout.contains("Started `db.serve`"), "{stdout}");

    std::fs::remove_file(&fail).expect("repair");
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x", "--to", "ckpt_001"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("  resource: db command + restarted ok\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  resource: web none + restarted ok\n"),
        "{stdout}"
    );
    assert!(stdout.ends_with("ready: `x`\n"), "{stdout}");
    let (_, status, _) = newgit(&repo, &["status", "x"]);
    assert!(status.contains("db:running web:running"), "{status}");
}
