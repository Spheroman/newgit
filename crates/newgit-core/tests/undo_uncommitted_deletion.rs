//! Undo must bring back an uncommitted deletion of a tracked file, the same
//! as it brings back an uncommitted edit: a checkpoint "protects the
//! worktree as it stands, not just what the agent remembered to commit".
//!
//! The checkpoint's dirty commit records the deletion, but
//! `GitSource::workspace_restore_to` first resets hard to the checkpoint's
//! HEAD — which has the file — and then reapplies the dirty commit. That
//! used to be a plain `git checkout <dirty> -- .`, whose overlay mode only
//! writes the paths the dirty tree has and never removes one it lacks: the
//! file came back with its committed content, and with every tracked file
//! deleted there was no path to write, so the checkout and the undo failed.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, UndoOptions};
use newgit_core::source::GitSource;
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

fn git_stdout(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join(".gitignore"), ".env.local\n").expect("write");
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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

#[test]
fn undo_restores_an_uncommitted_deletion_of_a_tracked_file() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = BranchManager::open(store).expect("manager");
    let ws = m
        .spawn("feature-a", None)
        .expect("spawn")
        .branch
        .workspace_path;

    // The agent deletes a tracked file and does not commit.
    std::fs::remove_file(ws.join("README.md")).expect("rm");
    let at_checkpoint = git_stdout(&ws, &["status", "--porcelain"]);
    let checkpoint = m
        .checkpoint("feature-a", Some("README deleted"))
        .expect("checkpoint");
    assert!(
        checkpoint.record.source.dirty_rev.is_some(),
        "precondition: the deletion is uncommitted state"
    );

    // Then it changes its mind, and the checkpoint is what it goes back to.
    std::fs::write(ws.join("README.md"), "recreated\n").expect("write");
    let undone = m.undo("feature-a", &UndoOptions::default()).expect("undo");
    assert_eq!(undone.restored.id, checkpoint.record.id);

    let after_undo = git_stdout(&ws, &["status", "--porcelain"]);
    assert!(
        !ws.join("README.md").exists(),
        "undo to {} left README.md on disk ({:?}), but it was deleted when that checkpoint \
         was taken.\n  git status at the checkpoint: {at_checkpoint:?}\n  git status after \
         undo:      {after_undo:?}",
        checkpoint.record.id,
        std::fs::read_to_string(ws.join("README.md")).ok()
    );
    // Deleted and uncommitted, as it was — not staged, not committed.
    assert_eq!(after_undo, at_checkpoint);
}

#[test]
fn undo_restores_a_checkpoint_that_deleted_every_tracked_file() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = BranchManager::open(store).expect("manager");
    let ws = m
        .spawn("feature-a", None)
        .expect("spawn")
        .branch
        .workspace_path;

    for path in git_stdout(&ws, &["ls-files"]).lines() {
        std::fs::remove_file(ws.join(path)).expect("rm");
    }
    let at_checkpoint = git_stdout(&ws, &["status", "--porcelain"]);
    let checkpoint = m
        .checkpoint("feature-a", Some("everything deleted"))
        .expect("checkpoint");
    let dirty = checkpoint
        .record
        .source
        .dirty_rev
        .as_deref()
        .expect("precondition: the deletions are uncommitted state");
    assert_eq!(
        git_stdout(&ws, &["ls-tree", "-r", "--name-only", dirty]),
        "",
        "precondition: the dirty commit's tree is empty, so the reapply has no path to write"
    );

    std::fs::write(ws.join("README.md"), "recreated\n").expect("write");
    let undone = m.undo("feature-a", &UndoOptions::default()).expect("undo");
    assert_eq!(undone.restored.id, checkpoint.record.id);
    assert_eq!(git_stdout(&ws, &["status", "--porcelain"]), at_checkpoint);
}

#[test]
fn restore_keeps_a_case_only_rename_on_a_case_insensitive_file_system() {
    // `spawn --from-checkpoint` restores checkpoints made on other machines.
    // Where case matters, `readme.md` renamed to `README.md` is a deletion
    // and an addition; where it does not, they are one file, and removing
    // the old name must not take the new one with it. Where case matters
    // this holds either way.
    let (_guard, temp) = tempdir();
    let ws = temp.join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir");
    git(&ws, &["init", "-q", "-b", "main"]);
    std::fs::write(ws.join("readme.md"), "hello\n").expect("write");
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-q", "-m", "initial"]);
    let head = git_stdout(&ws, &["rev-parse", "HEAD"]);

    // Built by hand: a case-insensitive file system cannot record it.
    let blob = git_stdout(&ws, &["rev-parse", "HEAD:readme.md"]);
    git(&ws, &["rm", "-q", "--cached", "readme.md"]);
    git(
        &ws,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},README.md"),
        ],
    );
    let tree = git_stdout(&ws, &["write-tree"]);
    let dirty = git_stdout(
        &ws,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            "rename",
        ],
    );

    GitSource::workspace_restore_to(&ws, &head, Some(&dirty), &[], &[]).expect("restore");

    let mut on_disk: Vec<_> = std::fs::read_dir(&ws)
        .expect("read_dir")
        .map(|entry| entry.expect("entry").file_name())
        .filter(|name| name != ".git")
        .collect();
    on_disk.sort();
    assert_eq!(on_disk, ["README.md"]);
    assert_eq!(
        std::fs::read_to_string(ws.join("README.md")).expect("read"),
        "hello\n"
    );
}
