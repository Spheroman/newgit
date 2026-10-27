//! Regression test for #211: `newgit undo` printed `resource: deps none ok`
//! for a resource whose `prepare` had failed and that the undo left failed.
//! The `ok` is the verdict on the restore action, which ran nothing and so
//! could not fail; under "Restored" it read as the resource being fine
//! while `newgit status` showed `deps:failed`. The line now says the status
//! the resource is left in.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

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

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops; matches `spawn_exit.rs` rather than pulling in the
/// `tempfile` dev-dependency the core crate uses.
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
        "newgit-undo-left-status-{}-{n}",
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

fn newgit(repo: &Utf8Path, args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status,
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8(output.stderr).expect("utf8 stderr"),
    )
}

/// The issue's resource: a `prepare` that fails and no `[restore]`, so an
/// undo runs nothing for it.
const NEVER_PREPARES: &str = r#"ownership = "branch"

[actions.prepare]
command = "exit 1"
"#;

const FINE: &str = r#"ownership = "branch"

[actions.prepare]
command = "true"
"#;

const DEPENDENT: &str = r#"ownership = "branch"
depends_on = ["deps"]

[actions.prepare]
command = "true"
"#;

#[test]
fn undo_says_what_status_a_resource_is_left_in() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", NEVER_PREPARES);
    write_resource(&store, "fine", FINE);
    write_resource(&store, "dependent", DEPENDENT);
    let repo = store.paths().project_root.clone();

    let (status, stdout, _) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(status.code(), Some(1), "deps fails to bind:\n{stdout}");
    let (status, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert!(status.success(), "checkpoint:\n{stdout}\n{stderr}");

    let (status, stdout, stderr) = newgit(&repo, &["undo", "x"]);
    assert!(status.success(), "undo:\n{stdout}\n{stderr}");
    // Still "Restored": the undo did everything it set out to do, and the
    // restore verdict is still `ok` — it just does not stop there.
    assert!(stdout.contains("Restored `x` to ckpt_001"), "{stdout}");
    assert!(
        stdout.contains("  resource: deps none ok, left failed\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  resource: dependent none ok, left blocked by deps\n"),
        "{stdout}"
    );
    // A resource the undo leaves ready says nothing more than it did.
    assert!(stdout.contains("  resource: fine none ok\n"), "{stdout}");

    // And the line agrees with `status`.
    let (_, stdout, _) = newgit(&repo, &["status", "x"]);
    assert!(stdout.contains("deps:failed"), "{stdout}");
    assert!(stdout.contains("fine:ready"), "{stdout}");
    assert!(stdout.contains("dependent:blocked(deps)"), "{stdout}");
}
