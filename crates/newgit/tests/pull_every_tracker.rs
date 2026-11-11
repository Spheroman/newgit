//! `newgit pull` with no arguments (#345), as typed: it is no longer an
//! error, says when nothing moved, and otherwise names each tracker its one
//! commit moved, on the branch it ran on.

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
        "newgit-pull-every-tracker-{}-{n}",
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

/// A project with `lib` and `ui` carved and published, and `env`, a tracker
/// outside Git.
fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("libs/lib")).expect("mkdir");
    std::fs::create_dir_all(repo.join("libs/ui")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("libs/lib/index.js"), "one\n").expect("write");
    std::fs::write(repo.join("libs/ui/index.js"), "one\n").expect("write");
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
    for (tracker, mount) in [("lib", "libs/lib"), ("ui", "libs/ui")] {
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

/// Elsewhere: one commit per file on `tracker`'s `main`.
fn advance(temp: &Utf8Path, store: &MetadataStore, tracker: &str, files: &[&str]) {
    let tracker_store = store.paths().tracker_store.clone();
    let elsewhere = temp.join(format!("elsewhere-{tracker}"));
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    git(&elsewhere, &["init", "-q", "-b", "main"]);
    git(&elsewhere, &["config", "commit.gpgSign", "false"]);
    git(
        &elsewhere,
        &[
            "fetch",
            "-q",
            tracker_store.as_str(),
            &format!("+refs/trackers/{tracker}/main:refs/remotes/store/main"),
        ],
    );
    git(
        &elsewhere,
        &["reset", "-q", "--hard", "refs/remotes/store/main"],
    );
    for file in files {
        std::fs::write(elsewhere.join(file), "x\n").expect("write");
        git(&elsewhere, &["add", "-A"]);
        git(
            &elsewhere,
            &["commit", "-q", "-m", &format!("{tracker}: {file}")],
        );
    }
    git(
        &elsewhere,
        &[
            "push",
            "-q",
            tracker_store.as_str(),
            &format!("HEAD:refs/trackers/{tracker}/main"),
        ],
    );
}

#[test]
fn plain_pull_says_nothing_moved_then_names_each_tracker_it_moved() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let nothing = ok(&repo, &["pull"]);
    assert!(
        nothing.contains(
            "Nothing to pull: no mounted tracker is behind its `follow` ref in the project root"
        ),
        "{nothing}"
    );
    assert!(
        nothing.contains("trackers outside Git have no pins and were left alone (`env`)"),
        "{nothing}"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before, "no commit");

    advance(&temp, &store, "lib", &["a.js", "b.js"]);
    advance(&temp, &store, "ui", &["c.js"]);
    let pulled = ok(&repo, &["pull"]);
    assert!(
        pulled.contains("Updated 2 trackers in the project root, one commit"),
        "{pulled}"
    );
    assert!(
        pulled.contains("lib          2 commits from main"),
        "{pulled}"
    );
    assert!(
        pulled.contains("ui           1 commit from main"),
        "{pulled}"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Update trackers: lib (2 commits), ui (1 commit)"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD^"]), before);
}

#[test]
fn plain_pull_in_a_workspace_lands_on_that_instance() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    ok(&repo, &["spawn", "feature"]);
    let workspace = Utf8PathBuf::from(ok(&repo, &["status", "feature", "--path"]).trim());
    let project_head = git(&repo, &["rev-parse", "HEAD"]);
    advance(&temp, &store, "ui", &["c.js"]);

    let pulled = ok(&workspace, &["pull"]);
    assert!(
        pulled.contains("Updated 1 tracker in `feature`, one commit"),
        "{pulled}"
    );
    assert_eq!(
        git(&workspace, &["log", "-1", "--format=%s"]),
        "Update trackers: ui (1 commit)"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        project_head,
        "the project root stays on its pins"
    );
}
