//! `tracker drop --discard` checkpoints a code tracker's uncommitted work so
//! `newgit undo` gets it back. That has to hold for a deleted file too: the
//! checkpoint records the deletion, and undoing to it must leave the file
//! deleted, not bring it back from HEAD.
//!
//! Undo used to restore the source while the tracker was still dropped, when
//! its paths were skip-worktree entries the restore's `--no-overlay` removal
//! leaves alone, and only then bind the tracker again — which checked the
//! file out from the index (#210). The binding now comes back first.
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

#[test]
fn undo_after_discarding_drop_keeps_the_deletion_the_checkpoint_holds() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let workspace = manager
        .spawn("x", None)
        .expect("spawn")
        .branch
        .workspace_path;

    std::fs::remove_file(workspace.join("apps/b/index.js")).expect("rm");
    std::fs::write(workspace.join("apps/b/keep.js"), "keep v2\n").expect("write");
    std::fs::write(workspace.join("apps/b/new.js"), "new\n").expect("write");
    std::fs::write(workspace.join("apps/a/index.js"), "v2\n").expect("write");
    let at_checkpoint = git(
        &workspace,
        &["status", "--porcelain", "--untracked-files=all"],
    );

    let refused = manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect_err("uncommitted work refuses a plain drop");
    assert!(refused.to_string().contains("--discard"), "{refused}");

    let change = manager
        .drop_tracker("x", "b", Uncommitted::Discard)
        .expect("drop with --discard");
    let checkpoint = change.checkpoint.expect("--discard checkpoints first");
    assert!(
        !workspace.join("apps/b").exists(),
        "apps/b left the working tree"
    );

    let undone = manager.undo("x", &UndoOptions::default()).expect("undo");
    assert_eq!(undone.restored.id, checkpoint);
    let branch = manager.store().find_branch("x").expect("instance");
    assert!(!branch.dropped.contains("b"), "{:?}", branch.dropped);

    let after_undo = git(
        &workspace,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    assert!(
        !workspace.join("apps/b/index.js").exists(),
        "undo to {checkpoint} brought back apps/b/index.js ({:?}), but it was deleted when \
         that checkpoint was taken.\n  git status at the checkpoint: {at_checkpoint:?}\n  git \
         status after undo:      {after_undo:?}",
        std::fs::read_to_string(workspace.join("apps/b/index.js")).ok()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/b/keep.js")).expect("read"),
        "keep v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/b/new.js")).expect("read"),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/a/index.js")).expect("read"),
        "v2\n"
    );
    assert_eq!(after_undo, at_checkpoint);
}
