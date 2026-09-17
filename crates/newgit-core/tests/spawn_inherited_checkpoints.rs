//! Checkpoint numbering is per name and survives `remove`, because `remove`
//! archives the binding record without deleting the checkpoint files. A fresh
//! instance under a removed name therefore starts at `ckpt_004`, which is the
//! one thing that carried over when the workspace, containers and volumes did
//! not (#84). `spawn` now says so; these tests pin when it does and does not.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::manager::BranchManager;
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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
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
    config.workspace = Some(newgit_core::config::WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
}

fn manager_at(store: &MetadataStore) -> BranchManager {
    BranchManager::open(MetadataStore::at(store.paths().project_root.clone())).expect("manager")
}

#[test]
fn a_first_spawn_inherits_nothing() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = manager_at(&store);

    let spawned = manager.spawn("feature-a", None).expect("spawn");
    assert_eq!(spawned.inherited_checkpoints, None);
}

#[test]
fn respawning_a_removed_name_reports_the_numbering_it_continues() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = manager_at(&store);

    manager.spawn("feature-a", None).expect("spawn");
    manager.checkpoint("feature-a", Some("v1")).expect("ckpt 1");
    manager.checkpoint("feature-a", Some("v2")).expect("ckpt 2");
    manager.checkpoint("feature-a", Some("v3")).expect("ckpt 3");
    manager
        .remove("feature-a", &temp, ArchivedCheckpoints::Keep)
        .expect("remove");

    // A brand-new instance: new workspace, new record, nothing carried over
    // but the numbering.
    let respawned = manager.spawn("feature-a", None).expect("respawn");
    let inherited = respawned
        .inherited_checkpoints
        .expect("respawn inherits the archived numbering");
    assert_eq!(inherited.existing, 3);
    assert_eq!(inherited.next_id, "ckpt_004");

    // And the claim is true: the next checkpoint really does get that id.
    let outcome = manager.checkpoint("feature-a", None).expect("checkpoint");
    assert_eq!(outcome.record.id, "ckpt_004");
}

/// The report is about a *previous* instance, so it must not fire for
/// checkpoints this instance took itself — a name spawned once and
/// checkpointed has nothing to explain.
#[test]
fn a_different_name_starts_clean_even_after_another_name_was_removed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = manager_at(&store);

    manager.spawn("feature-a", None).expect("spawn");
    manager.checkpoint("feature-a", Some("v1")).expect("ckpt");
    manager
        .remove("feature-a", &temp, ArchivedCheckpoints::Keep)
        .expect("remove");

    let other = manager.spawn("feature-b", None).expect("spawn b");
    assert_eq!(other.inherited_checkpoints, None);
}
