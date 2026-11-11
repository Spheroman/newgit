//! `newgit pull lib feature`, the form from before the instance became
//! `--instance`: `feature` is an instance, and the error says so and gives
//! the form that works.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "ana")
        .env("GIT_AUTHOR_EMAIL", "ana@example.com")
        .env("GIT_COMMITTER_NAME", "ana")
        .env("GIT_COMMITTER_EMAIL", "ana@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops — the same hand-rolled guard as `tracker_log.rs`.
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
        "newgit-pull-instance-hint-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn newgit(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "ana")
        .env("GIT_AUTHOR_EMAIL", "ana@example.com")
        .env("GIT_COMMITTER_NAME", "ana")
        .env("GIT_COMMITTER_EMAIL", "ana@example.com")
        .output()
        .expect("newgit runs")
}

fn ok(dir: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// A project with `lib`, `ui` and `api` carved and published, and `env`, a
/// tracker outside Git.
fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("libs/lib")).expect("mkdir");
    std::fs::create_dir_all(repo.join("libs/ui")).expect("mkdir");
    std::fs::create_dir_all(repo.join("libs/api")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("libs/lib/index.js"), "one\n").expect("write");
    std::fs::write(repo.join("libs/ui/index.js"), "one\n").expect("write");
    std::fs::write(repo.join("libs/api/index.js"), "one\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    std::fs::write(repo.join(".env.local"), "A=1\n").expect("write env");
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager
        .create_tracker("env", "user", Storage::Local, false)
        .expect("create env");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track env");
    manager.seed_tracker_from_store("env").expect("seed env");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);
    for (tracker, mount) in [("lib", "libs/lib"), ("ui", "libs/ui"), ("api", "libs/api")] {
        BranchManager::open(store.clone())
            .expect("manager")
            .create_git_tracker(tracker, "project-devs", &[Utf8PathBuf::from(mount)])
            .expect("carve");
    }
    BranchManager::open(store.clone())
        .expect("reopen")
        .merge("main", "main")
        .expect("publish");
    store
}

#[test]
fn an_instance_named_where_a_tracker_goes_gives_the_instance_form() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    ok(&repo, &["spawn", "feature"]);
    let output = newgit(&repo, &["pull", "lib", "feature"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("`feature` is an instance: `newgit pull lib --instance feature`"),
        "{stderr}"
    );
}
