//! A workspace off its instance's branch (#259).
//!
//! Agents routinely `git checkout -b try-x` inside a workspace. A checkpoint
//! used to record that branch's HEAD as the instance's source and move the
//! instance's branch in the store to it, silently; undo then reset whatever
//! branch the workspace was on, `try-x` included.
//!
//! Now a checkpoint holds the workspace as it stands — which branch is
//! checked out included — and the instance's branch moves only with commits
//! made on it. Undo puts HEAD back on the branch the checkpoint was taken on,
//! and `status` shows when the workspace is somewhere else.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::checkpoint::{OffBranch, SourceState};
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, UndoOptions};
use newgit_core::store::MetadataStore;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit(ws: &Utf8Path, file: &str, content: &str) -> String {
    std::fs::write(ws.join(file), content).expect("write");
    git(ws, &["add", file]);
    git(ws, &["commit", "-q", "-m", file]);
    git(ws, &["rev-parse", "HEAD"])
}

fn setup(temp: &Utf8Path) -> (MetadataStore, Utf8PathBuf) {
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
    (store, repo)
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn spawned(temp: &Utf8Path) -> (BranchManager, Utf8PathBuf, Utf8PathBuf) {
    let (store, repo) = setup(temp);
    let m = BranchManager::open(store).expect("manager");
    let ws = m.spawn("base", None).expect("spawn").branch.workspace_path;
    (m, ws, repo)
}

fn store_rev(repo: &Utf8Path, branch: &str) -> String {
    git(repo, &["rev-parse", &format!("refs/heads/{branch}")])
}

fn off_branch(m: &BranchManager) -> Option<OffBranch> {
    m.statuses()
        .expect("status")
        .into_iter()
        .find(|report| report.branch.name == "base")
        .expect("base reported")
        .off_branch
}

#[test]
fn a_checkpoint_on_another_branch_leaves_the_instance_branch_alone() {
    let (_guard, temp) = tempdir();
    let (m, ws, repo) = spawned(&temp);
    let on_base = commit(&ws, "a.txt", "a\n");
    m.checkpoint("base", None).expect("checkpoint on base");
    assert_eq!(store_rev(&repo, "base"), on_base);
    assert_eq!(off_branch(&m), None);

    git(&ws, &["checkout", "-q", "-b", "sneaky"]);
    let sneaky = commit(&ws, "s.txt", "s\n");
    let checkpoint = m.checkpoint("base", None).expect("checkpoint on sneaky");

    assert_eq!(
        store_rev(&repo, "base"),
        on_base,
        "the store's `base` must not take `sneaky`'s commits"
    );
    assert_eq!(
        m.store().find_branch("base").expect("record").source_rev,
        on_base
    );
    // The checkpoint still holds the workspace as it stands, and says where.
    assert_eq!(checkpoint.record.source.head_rev, sneaky);
    assert_eq!(
        checkpoint.record.source.checked_out.as_deref(),
        Some("sneaky")
    );
    assert!(!checkpoint.record.source.detached);
    assert!(
        checkpoint
            .warnings
            .iter()
            .any(|warning| warning.contains("`sneaky`") && warning.contains("`base`")),
        "the checkpoint names both branches: {:?}",
        checkpoint.warnings
    );
    assert_eq!(off_branch(&m), Some(OffBranch::Branch("sneaky".to_owned())));

    // Back on `base`, the next checkpoint moves it again, and nothing warns.
    git(&ws, &["checkout", "-q", "base"]);
    let later = commit(&ws, "b.txt", "b\n");
    let checkpoint = m.checkpoint("base", None).expect("checkpoint back on base");
    assert_eq!(store_rev(&repo, "base"), later);
    assert!(checkpoint.warnings.is_empty(), "{:?}", checkpoint.warnings);
    assert_eq!(off_branch(&m), None);
}

#[test]
fn undo_puts_the_workspace_back_on_the_checkpoints_branch_and_spares_the_one_it_was_on() {
    let (_guard, temp) = tempdir();
    let (m, ws, repo) = spawned(&temp);
    let on_base = commit(&ws, "a.txt", "a\n");
    let at_base = m.checkpoint("base", None).expect("checkpoint on base");

    git(&ws, &["checkout", "-q", "-b", "sneaky"]);
    let sneaky = commit(&ws, "s.txt", "s\n");

    let undone = m
        .undo(
            "base",
            &UndoOptions {
                to: Some(at_base.record.id.clone()),
                ..UndoOptions::default()
            },
        )
        .expect("undo");
    assert_eq!(git(&ws, &["branch", "--show-current"]), "base");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), on_base);
    assert_eq!(
        git(&ws, &["rev-parse", "refs/heads/sneaky"]),
        sneaky,
        "undo must not reset a branch the checkpoint was not taken on"
    );
    assert!(!ws.join("s.txt").exists());
    assert_eq!(store_rev(&repo, "base"), on_base);
    assert!(
        undone
            .warnings
            .iter()
            .any(|warning| warning.contains("`sneaky`") && warning.contains("`base`")),
        "undo says it switched branches: {:?}",
        undone.warnings
    );
    // The safety checkpoint was taken on `sneaky`, and says so.
    assert_eq!(undone.safety.source.checked_out.as_deref(), Some("sneaky"));

    // Redo: back on `sneaky`, as it was; `base` stays put.
    m.undo(
        "base",
        &UndoOptions {
            to: Some(undone.safety.id.clone()),
            ..UndoOptions::default()
        },
    )
    .expect("redo");
    assert_eq!(git(&ws, &["branch", "--show-current"]), "sneaky");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), sneaky);
    assert!(ws.join("s.txt").exists());
    assert_eq!(git(&ws, &["rev-parse", "refs/heads/base"]), on_base);
    assert_eq!(store_rev(&repo, "base"), on_base);
}

#[test]
fn undo_moving_a_branch_the_workspace_is_not_on_names_its_old_tip() {
    let (_guard, temp) = tempdir();
    let (m, ws, _repo) = spawned(&temp);
    git(&ws, &["checkout", "-q", "-b", "sneaky"]);
    let first = commit(&ws, "s.txt", "s1\n");
    let at_first = m.checkpoint("base", None).expect("checkpoint");
    let second = commit(&ws, "s.txt", "s2\n");
    git(&ws, &["checkout", "-q", "base"]);

    let undone = m
        .undo(
            "base",
            &UndoOptions {
                to: Some(at_first.record.id.clone()),
                ..UndoOptions::default()
            },
        )
        .expect("undo");
    assert_eq!(git(&ws, &["branch", "--show-current"]), "sneaky");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), first);
    assert!(
        undone
            .warnings
            .iter()
            .any(|warning| warning.contains(&second[..8]) && warning.contains("git branch")),
        "the commits undo took off `sneaky` are named: {:?}",
        undone.warnings
    );
}

#[test]
fn a_detached_head_is_checkpointed_and_restored_detached() {
    let (_guard, temp) = tempdir();
    let (m, ws, repo) = spawned(&temp);
    let on_base = commit(&ws, "a.txt", "a\n");
    m.checkpoint("base", None).expect("checkpoint on base");
    git(&ws, &["checkout", "-q", "--detach"]);
    let detached = commit(&ws, "d.txt", "d\n");

    let checkpoint = m.checkpoint("base", None).expect("checkpoint detached");
    assert!(checkpoint.record.source.detached);
    assert_eq!(checkpoint.record.source.checked_out, None);
    assert_eq!(checkpoint.record.source.head_rev, detached);
    assert_eq!(store_rev(&repo, "base"), on_base);
    assert!(
        checkpoint
            .warnings
            .iter()
            .any(|warning| warning.contains("detached")),
        "{:?}",
        checkpoint.warnings
    );
    assert_eq!(off_branch(&m), Some(OffBranch::Detached));

    git(&ws, &["checkout", "-q", "base"]);
    let later = commit(&ws, "b.txt", "b\n");
    m.undo(
        "base",
        &UndoOptions {
            to: Some(checkpoint.record.id.clone()),
            ..UndoOptions::default()
        },
    )
    .expect("undo");
    assert_eq!(git(&ws, &["branch", "--show-current"]), "");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), detached);
    assert_eq!(git(&ws, &["rev-parse", "refs/heads/base"]), later);
    assert_eq!(store_rev(&repo, "base"), later);
}

#[test]
fn a_record_from_before_the_branch_was_kept_reads_as_the_instance_branch() {
    let old = SourceState {
        head_rev: "abc".to_owned(),
        dirty_rev: None,
        store_ref: "refs/newgit/checkpoints/base/ckpt_001".to_owned(),
        checked_out: None,
        detached: false,
    };
    assert_eq!(old.head_branch("base"), Some("base"));
    assert_eq!(old.off_branch("base"), None);
}
