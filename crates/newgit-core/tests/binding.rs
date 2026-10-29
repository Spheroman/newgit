//! Binding (docs/design/trackers.md, step 4): `newgit tracker use` and `drop`. An
//! instance's history holds every code tracker its user may read; dropping one
//! takes it out of the working tree (sparse checkout) and out of the instance's
//! scope, never out of its history.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, TrackerMergeAction, Uncommitted};
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
fn a_dropped_code_tracker_leaves_the_working_tree_but_not_the_history() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "feature-a");
    let head = git(&workspace, &["rev-parse", "HEAD"]);
    manager
        .drop_tracker("feature-a", "electron", Uncommitted::Refuse)
        .expect("drop");
    assert!(!workspace.join("apps/electron/main.js").exists());
    assert!(workspace.join("README.md").exists());
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), head, "no commit");
    assert!(git(&workspace, &["status", "--porcelain"]).is_empty());

    manager.use_tracker("feature-a", "electron").expect("use");
    assert!(workspace.join("apps/electron/main.js").exists());
}

#[test]
fn uncommitted_work_refuses_a_drop_until_stashed_and_comes_back_with_use() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "feature-a");
    std::fs::write(workspace.join("apps/electron/main.js"), "edited\n").expect("write");
    std::fs::write(workspace.join("apps/electron/new.js"), "new\n").expect("write");
    let refused = manager
        .drop_tracker("feature-a", "electron", Uncommitted::Refuse)
        .expect_err("uncommitted");
    assert!(refused.to_string().contains("--stash"), "{refused}");

    let dropped = manager
        .drop_tracker("feature-a", "electron", Uncommitted::Stash)
        .expect("drop, stashing");
    assert!(dropped.stashed);
    assert!(!workspace.join("apps/electron/new.js").exists());

    let used = manager.use_tracker("feature-a", "electron").expect("use");
    assert!(used.restored_stash);
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/electron/main.js")).expect("read"),
        "edited\n"
    );
    assert!(workspace.join("apps/electron/new.js").exists());
}

#[test]
fn a_discarding_drop_checkpoints_first() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "feature-a");
    std::fs::write(workspace.join("apps/electron/main.js"), "edited\n").expect("write");
    let dropped = manager
        .drop_tracker("feature-a", "electron", Uncommitted::Discard)
        .expect("drop, discarding");
    let checkpoint = dropped.checkpoint.expect("a checkpoint");
    let record = manager
        .list_checkpoints("feature-a")
        .expect("checkpoints")
        .into_iter()
        .find(|record| record.id == checkpoint)
        .expect("recorded");
    assert!(
        record.source.dirty_rev.is_some(),
        "the discarded work is in it"
    );
}

#[test]
fn a_merge_leaves_a_dropped_tracker_at_the_branchs_commit() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "feature-a");
    let electron_main = git(
        &manager.store().paths().tracker_store,
        &["rev-parse", "refs/trackers/electron/main"],
    );
    manager
        .drop_tracker("feature-a", "electron", Uncommitted::Refuse)
        .expect("drop");
    std::fs::write(workspace.join("README.md"), "work\n").expect("write");
    git(&workspace, &["commit", "-qam", "readme"]);
    let outcome = manager.merge("feature-a", "main").expect("merge");
    let electron = outcome
        .trackers
        .iter()
        .find(|merge| merge.tracker == "electron")
        .expect("electron");
    assert_eq!(electron.action, TrackerMergeAction::Unchanged);
    assert_eq!(electron.commit, electron_main);
}

#[test]
fn a_dropped_secret_tracker_is_captured_then_removed_and_comes_back() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "feature-a");
    std::fs::write(workspace.join(".env.local"), "KEY=mine\n").expect("write");
    let dropped = manager
        .drop_tracker("feature-a", "env", Uncommitted::Refuse)
        .expect("drop");
    assert!(dropped.captured.is_some());
    assert!(!workspace.join(".env.local").exists());
    // A checkpoint while it is dropped does not record it as deleted.
    manager.checkpoint("feature-a", None).expect("checkpoint");

    manager.use_tracker("feature-a", "env").expect("use");
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("read"),
        "KEY=mine\n"
    );
}

#[test]
fn root_project_and_access_cannot_be_dropped() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    spawn(&manager, "feature-a");
    for tracker in ["root", "project", "access"] {
        let refused = manager
            .drop_tracker("feature-a", tracker, Uncommitted::Refuse)
            .expect_err(tracker);
        assert!(
            refused.to_string().contains("cannot be dropped"),
            "{refused}"
        );
    }
}
