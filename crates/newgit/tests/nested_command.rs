//! A hook that runs `newgit` on its own instance (#212). The command that
//! runs the hook holds the instance's lock, so the nested one would wait for
//! its own ancestor forever; told through `NEWGIT_LOCKED`, it refuses
//! instead, and says why.
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

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-nested-{}-{n}", std::process::id()));
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

fn newgit(repo: &Utf8Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .env_remove("NEWGIT_LOCKED")
        .args(args)
        .output()
        .expect("newgit runs")
}

#[test]
fn a_hook_running_newgit_on_its_own_instance_is_refused_not_deadlocked() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let nested_stderr = temp.join("nested.stderr");
    let binary = env!("CARGO_BIN_EXE_newgit");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!(
            r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "{binary} checkpoint x -m nested 2>{nested_stderr}; echo state"
"#
        ),
    )
    .expect("write resource");

    let spawned = newgit(&repo, &["spawn", "x"]);
    assert!(
        spawned.status.success(),
        "{}",
        String::from_utf8_lossy(&spawned.stdout)
    );
    // Would hang here before the lock was announced to hooks.
    let outer = newgit(&repo, &["checkpoint", "x", "-m", "outer"]);
    assert!(
        outer.status.success(),
        "{}{}",
        String::from_utf8_lossy(&outer.stdout),
        String::from_utf8_lossy(&outer.stderr)
    );

    let nested = std::fs::read_to_string(&nested_stderr).expect("nested stderr");
    assert!(
        nested.contains("started from inside it"),
        "the nested checkpoint did not refuse: {nested}"
    );
    // Only the outer checkpoint exists.
    let list = newgit(&repo, &["checkpoints", "x"]);
    let out = String::from_utf8_lossy(&list.stdout);
    assert!(out.contains("ckpt_001"), "{out}");
    assert!(!out.contains("ckpt_002"), "{out}");
}
