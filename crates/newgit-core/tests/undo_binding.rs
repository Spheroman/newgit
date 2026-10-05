//! Undo restores the binding too: a checkpoint records which trackers the
//! instance did not bind, and undoing to it puts that back with everything
//! else.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, TrackerMergeAction, Uncommitted, UndoOptions};
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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn setup(temp: &Utf8Path) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::create_dir_all(repo.join("apps/electron")).expect("mkdir");
    std::fs::write(repo.join("apps/electron/main.js"), "v1\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);
    BranchManager::open(store.clone())
        .expect("manager")
        .create_code_tracker(
            "electron",
            "project-devs",
            &[Utf8PathBuf::from("apps/electron")],
        )
        .expect("carve");
    let manager = BranchManager::open(store.clone()).expect("reopen");
    manager
        .create_tracker("env", "user", Storage::Local, false)
        .expect("env");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    let manager = BranchManager::open(store).expect("manager");
    // The project root's own commits reach `main` like anyone's.
    let published = manager
        .merge("main", "main")
        .expect("merge the project root");
    assert!(
        published
            .trackers
            .iter()
            .any(|merge| merge.tracker == "project" && merge.action == TrackerMergeAction::New),
        "{:?}",
        published.trackers
    );
    manager
}

fn spawn(manager: &BranchManager, name: &str) -> Utf8PathBuf {
    manager.spawn(name, None).expect("spawn");
    manager
        .store()
        .find_branch(name)
        .expect("instance")
        .workspace_path
}

#[test]
fn undo_brings_back_trackers_dropped_since() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "feature-a");
    std::fs::write(workspace.join(".env.local"), "KEY=mine\n").expect("write");
    manager.checkpoint("feature-a", None).expect("checkpoint");

    manager
        .drop_tracker("feature-a", "electron", Uncommitted::Refuse)
        .expect("drop electron");
    manager
        .drop_tracker("feature-a", "env", Uncommitted::Refuse)
        .expect("drop env");
    manager
        .undo("feature-a", &UndoOptions::default())
        .expect("undo");

    let branch = manager.store().find_branch("feature-a").expect("instance");
    assert!(branch.dropped.is_empty(), "{:?}", branch.dropped);
    assert!(workspace.join("apps/electron/main.js").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("read"),
        "KEY=mine\n"
    );
}

#[test]
fn undo_drops_again_what_was_used_since() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "feature-a");
    manager
        .drop_tracker("feature-a", "electron", Uncommitted::Refuse)
        .expect("drop");
    manager.checkpoint("feature-a", None).expect("checkpoint");
    manager.use_tracker("feature-a", "electron").expect("use");
    assert!(workspace.join("apps/electron/main.js").exists());

    manager
        .undo("feature-a", &UndoOptions::default())
        .expect("undo");
    let branch = manager.store().find_branch("feature-a").expect("instance");
    assert!(branch.dropped.contains("electron"));
    assert!(!workspace.join("apps/electron/main.js").exists());
}
