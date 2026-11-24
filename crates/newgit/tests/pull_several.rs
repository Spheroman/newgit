//! `newgit pull <tracker>...` (#426): every argument is a tracker, so
//! `newgit pull lib ui` pulls those two and nothing else, and `--instance`
//! names an instance other than the one it runs in. A named tracker that
//! does not move for a reason other than being up to date exits non-zero.
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
    let dir = std::env::temp_dir().join(format!("newgit-pull-several-{}-{n}", std::process::id()));
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

fn fails(dir: &Utf8Path, args: &[&str]) -> (String, String) {
    let output = newgit(dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "newgit {args:?} should fail:\n{stdout}"
    );
    (stdout, stderr)
}

#[test]
fn every_argument_is_a_tracker() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    advance(&temp, &store, "lib", &["a.js"]);
    advance(&temp, &store, "ui", &["b.js", "c.js"]);
    advance(&temp, &store, "api", &["d.js"]);
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let pulled = ok(&repo, &["pull", "lib", "ui"]);
    assert!(
        pulled.contains("Updated 2 trackers in the project root"),
        "{pulled}"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD^"]), before, "one commit");
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Update trackers: lib (1 commit), ui (2 commits)"
    );
    assert!(repo.join("libs/ui/c.js").is_file());
    assert!(!repo.join("libs/api/d.js").exists(), "api was not named");

    let rest = ok(&repo, &["pull"]);
    assert!(rest.contains("Updated 1 tracker"), "{rest}");
    assert!(repo.join("libs/api/d.js").is_file());
}

#[test]
fn instance_is_a_flag() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    ok(&repo, &["spawn", "feature"]);
    let workspace = Utf8PathBuf::from(ok(&repo, &["status", "feature", "--path"]).trim());
    advance(&temp, &store, "lib", &["a.js"]);
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let pulled = ok(&repo, &["pull", "lib", "--instance", "feature"]);
    assert!(
        pulled.contains("Updated 1 tracker in `feature`"),
        "{pulled}"
    );
    assert!(workspace.join("libs/lib/a.js").is_file());
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        before,
        "the project root stays"
    );

    // A second positional is a tracker, never an instance.
    let (_, stderr) = fails(&repo, &["pull", "lib", "feature"]);
    assert!(stderr.contains("feature"), "{stderr}");
}

#[test]
fn a_named_tracker_that_stays_exits_non_zero_and_plain_pull_does_not() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    ok(&repo, &["spawn", "feature"]);
    let workspace = Utf8PathBuf::from(ok(&repo, &["status", "feature", "--path"]).trim());
    std::fs::write(workspace.join("libs/lib/mine.js"), "mine\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "lib: mine"]);
    advance(&temp, &store, "lib", &["a.js"]);
    advance(&temp, &store, "ui", &["b.js"]);

    let (stdout, stderr) = fails(&workspace, &["pull", "lib"]);
    assert!(stdout.contains("`lib` stays"), "{stdout}");
    assert!(stderr.contains("not pulled: `lib`"), "{stderr}");

    let pulled = ok(&workspace, &["pull"]);
    assert!(pulled.contains("Updated 1 tracker"), "{pulled}");
    assert!(pulled.contains("`lib` stays"), "{pulled}");
    assert!(workspace.join("libs/ui/b.js").is_file());
    assert!(workspace.join("libs/lib/mine.js").is_file());
}
