//! Regression test for #86: `newgit spawn` printed `prepare: FAILED` for a
//! resource that never bound, then exited 0. Anything scripted (CI, a
//! wrapper, an agent driving newgit) needs the exit code to say what the
//! human-readable summary already says.
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
/// returned guard drops. Hand-rolled rather than pulling in the `tempfile`
/// dev-dependency other crates use — matches `table_alignment.rs`, the only
/// other test file in this crate.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-spawn-exit-{}-{n}", std::process::id()));
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

struct SpawnResult {
    status: std::process::ExitStatus,
    stdout: String,
}

fn run_spawn(repo: &Utf8Path, name: &str) -> SpawnResult {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(["spawn", name])
        .output()
        .expect("newgit runs");
    SpawnResult {
        status: output.status,
        stdout: String::from_utf8(output.stdout).expect("utf8 stdout"),
    }
}

const OK_RESOURCE: &str = r#"ownership = "branch"

[actions.prepare]
command = "true"
"#;

const FAILING_RESOURCE: &str = r#"ownership = "branch"

[actions.prepare]
command = "false"
"#;

#[test]
fn spawn_exits_zero_when_every_resource_binds() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", OK_RESOURCE);
    let repo = store.paths().project_root.clone();

    let result = run_spawn(&repo, "feature-a");
    assert!(
        result.status.success(),
        "expected exit 0, got {:?}\nstdout:\n{}",
        result.status.code(),
        result.stdout
    );
    assert!(result.stdout.contains("prepare: ok"), "{}", result.stdout);
}

#[test]
fn spawn_exits_nonzero_when_a_resource_fails_to_bind_and_still_writes_the_record() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "perf-a", FAILING_RESOURCE);
    let repo = store.paths().project_root.clone();

    let result = run_spawn(&repo, "feature-a");
    assert_eq!(
        result.status.code(),
        Some(1),
        "expected exit 1, got {:?}\nstdout:\n{}",
        result.status.code(),
        result.stdout
    );
    assert!(
        result.stdout.contains("prepare: FAILED"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("spawned with failures"),
        "expected a summary line naming the failure, got:\n{}",
        result.stdout
    );

    // Exit 1 reports the state; it does not roll the spawn back. The
    // instance and its record must still exist so a caller can inspect or
    // retry it, exactly as the human-readable output already says.
    let branches = store.load_branches().expect("load branches");
    assert!(
        branches.iter().any(|branch| branch.name == "feature-a"),
        "instance record should still exist after a failed bind"
    );
}

#[test]
fn spawn_exits_nonzero_when_a_dependent_is_blocked() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "base", FAILING_RESOURCE);
    write_resource(
        &store,
        "dependent",
        r#"ownership = "branch"
depends_on = ["base"]

[actions.prepare]
command = "true"
"#,
    );
    let repo = store.paths().project_root.clone();

    let result = run_spawn(&repo, "feature-a");
    assert_eq!(
        result.status.code(),
        Some(1),
        "a blocked prepare never ran, so the resource is no more usable than a \
         failed one — got {:?}\nstdout:\n{}",
        result.status.code(),
        result.stdout
    );
    assert!(result.stdout.contains("BLOCKED"), "{}", result.stdout);
}
