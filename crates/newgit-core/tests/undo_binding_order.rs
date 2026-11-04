//! Undo restores the binding before the source (#210), and that order has
//! edges beyond the issue's case: a tracker the checkpoint had dropped goes
//! again (directory and all, on a redo too), an undo that fails early leaves the record and the workspace
//! agreeing, the resources a rebound tracker brings are prepared against the
//! restored source, and a commit made after the drop does not change any of
//! it.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, TrackerMergeAction, Uncommitted, UndoOptions};
use newgit_core::store::MetadataStore;

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

/// Code trackers `a` and `b` over `apps/a` and `apps/b`. `b` brings a
/// resource `app` whose `prepare` appends what `apps/a/index.js` holds to
/// `temp/witness`, outside every workspace.
fn setup(temp: &Utf8Path) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    for app in ["a", "b"] {
        std::fs::create_dir_all(repo.join("apps").join(app)).expect("mkdir");
        std::fs::write(repo.join("apps").join(app).join("index.js"), "v1\n").expect("write");
    }
    std::fs::write(repo.join("apps/b/keep.js"), "keep v1\n").expect("write");
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
    for app in ["a", "b"] {
        BranchManager::open(store.clone())
            .expect("manager")
            .create_code_tracker(
                app,
                "project-devs",
                &[Utf8PathBuf::from(format!("apps/{app}"))],
            )
            .expect("carve");
    }
    let b = store.paths().trackers.join("b.toml");
    let mut definition = std::fs::read_to_string(&b).expect("read");
    definition.push_str("resources = [\"app\"]\n");
    std::fs::write(&b, definition).expect("write");
    std::fs::write(
        store.paths().resources.join("app.toml"),
        format!(
            "ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"cat apps/a/index.js >> \
             {}\"\n",
            temp.join("witness")
        ),
    )
    .expect("resource");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    let manager = BranchManager::open(store).expect("manager");
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

fn status(workspace: &Utf8Path) -> String {
    git(
        workspace,
        &["status", "--porcelain", "--untracked-files=all"],
    )
}

#[test]
fn undo_to_a_checkpoint_that_had_the_tracker_dropped_drops_it_again() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "x");
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop");
    manager.checkpoint("x", None).expect("checkpoint");
    let at_checkpoint = status(&workspace);
    manager.use_tracker("x", "b").expect("use");
    assert!(workspace.join("apps/b/index.js").exists());

    manager.undo("x", &UndoOptions::default()).expect("undo");

    let branch = manager.store().find_branch("x").expect("instance");
    assert!(branch.dropped.contains("b"), "{:?}", branch.dropped);
    assert!(
        !branch.resources.contains_key("app"),
        "{:?}",
        branch.resources
    );
    assert!(
        !workspace.join("apps/b").exists(),
        "apps/b left the working tree, directory and all"
    );
    assert_eq!(status(&workspace), at_checkpoint);
}

#[test]
fn an_undo_that_cannot_fetch_its_checkpoint_changes_no_binding() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "x");
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop");
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    manager.use_tracker("x", "b").expect("use");
    git(
        &temp.join("store"),
        &["update-ref", "-d", &checkpoint.record.source.store_ref],
    );

    manager
        .undo("x", &UndoOptions::default())
        .expect_err("the checkpoint's source is gone");

    // The record and the workspace still agree that `b` is bound.
    let branch = manager.store().find_branch("x").expect("instance");
    assert!(branch.dropped.is_empty(), "{:?}", branch.dropped);
    assert!(
        branch.resources.contains_key("app"),
        "{:?}",
        branch.resources
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/b/index.js")).expect("read"),
        "v1\n"
    );
    assert_eq!(status(&workspace), "");
}

#[test]
fn a_rebound_trackers_resources_are_prepared_against_the_restored_source() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "x");
    std::fs::write(workspace.join("apps/a/index.js"), "v2\n").expect("write");
    manager.checkpoint("x", None).expect("checkpoint");
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop");
    std::fs::write(workspace.join("apps/a/index.js"), "v3\n").expect("write");
    std::fs::write(temp.join("witness"), "").expect("reset the witness");

    manager.undo("x", &UndoOptions::default()).expect("undo");

    assert_eq!(
        std::fs::read_to_string(temp.join("witness")).expect("read"),
        "v2\n",
        "`prepare` ran before the source was back"
    );
    let branch = manager.store().find_branch("x").expect("instance");
    assert!(
        branch.resources.contains_key("app"),
        "{:?}",
        branch.resources
    );
}

#[test]
fn a_commit_after_the_drop_does_not_bring_the_deletion_back() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "x");
    let checkpoint_head = git(&workspace, &["rev-parse", "HEAD"]);
    std::fs::remove_file(workspace.join("apps/b/index.js")).expect("rm");
    std::fs::write(workspace.join("apps/b/keep.js"), "keep v2\n").expect("write");
    let at_checkpoint = status(&workspace);
    manager
        .drop_tracker("x", "b", Uncommitted::Discard)
        .expect("drop with --discard");

    std::fs::write(workspace.join("apps/a/index.js"), "v2\n").expect("write");
    git(&workspace, &["commit", "-q", "-am", "after the drop"]);
    assert_ne!(git(&workspace, &["rev-parse", "HEAD"]), checkpoint_head);

    manager.undo("x", &UndoOptions::default()).expect("undo");

    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), checkpoint_head);
    assert!(
        !workspace.join("apps/b/index.js").exists(),
        "the deletion the checkpoint holds came back"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/b/keep.js")).expect("read"),
        "keep v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/a/index.js")).expect("read"),
        "v1\n"
    );
    assert_eq!(status(&workspace), at_checkpoint);
}

#[test]
fn redo_after_undoing_a_discarding_drop_takes_the_tracker_out_again() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = spawn(&manager, "x");
    std::fs::remove_file(workspace.join("apps/b/index.js")).expect("rm");
    std::fs::write(workspace.join("apps/b/keep.js"), "keep v2\n").expect("write");
    std::fs::write(workspace.join("apps/b/new.js"), "new\n").expect("write");
    manager
        .drop_tracker("x", "b", Uncommitted::Discard)
        .expect("drop with --discard");
    let dropped = status(&workspace);
    manager.undo("x", &UndoOptions::default()).expect("undo");

    // Undo twice is redo: back to the drop, uncommitted work in `b` and all.
    manager.undo("x", &UndoOptions::default()).expect("redo");

    let branch = manager.store().find_branch("x").expect("instance");
    assert!(branch.dropped.contains("b"), "{:?}", branch.dropped);
    assert!(
        !workspace.join("apps/b").exists(),
        "apps/b left the working tree, directory and all"
    );
    assert_eq!(status(&workspace), dropped);
}
