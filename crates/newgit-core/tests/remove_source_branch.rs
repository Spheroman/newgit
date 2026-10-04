//! #82: `remove` deletes the source branch when it holds nothing unique, and
//! keeps it — saying why — when it might.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::manager::{BranchManager, KeptBranch, SourceBranchOutcome, SourceBranchPolicy};
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

fn branch_rev(repo: &Utf8Path, name: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo.as_str())
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ])
        .output()
        .expect("git runs");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn setup(temp: &Utf8Path) -> Utf8PathBuf {
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
    // Commit the init so the store's checkout is clean for `git switch`.
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit init"]);
    repo
}

fn manager(repo: &Utf8Path) -> BranchManager {
    BranchManager::open(MetadataStore::at(repo.to_path_buf())).expect("manager")
}

/// Spawn `name`, commit one file in its workspace, and checkpoint so the
/// store's branch carries the commit.
fn spawn_with_commit(repo: &Utf8Path, name: &str) -> String {
    let manager = manager(repo);
    let spawned = manager.spawn(name, None).expect("spawn");
    let workspace = spawned.branch.workspace_path;
    std::fs::write(workspace.join("work.txt"), "work\n").expect("write");
    git(&workspace, &["add", "work.txt"]);
    git(&workspace, &["commit", "-q", "-m", "work"]);
    manager.checkpoint(name, None).expect("checkpoint");
    git(&workspace, &["rev-parse", "HEAD"])
}

fn remove(
    repo: &Utf8Path,
    name: &str,
    policy: SourceBranchPolicy,
) -> newgit_core::Result<newgit_core::manager::RemoveOutcome> {
    manager(repo).remove(name, repo, ArchivedCheckpoints::Keep, policy)
}

#[test]
fn a_branch_with_no_commits_of_its_own_is_deleted() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    manager(&repo).spawn("smoke", None).expect("spawn");

    let outcome = remove(&repo, "smoke", SourceBranchPolicy::Auto).expect("remove");
    let SourceBranchOutcome::Deleted { found_on, .. } = outcome.source_branch else {
        panic!("expected deletion, got {:?}", outcome.source_branch);
    };
    assert_eq!(found_on.as_deref(), Some("`main`"));
    assert_eq!(branch_rev(&repo, "smoke"), None);

    // The point of the issue: the next spawn of the same name starts fresh
    // from HEAD rather than silently adopting the leftover branch.
    let respawned = manager(&repo).spawn("smoke", None).expect("respawn");
    assert!(respawned.created_source_branch);
    assert!(respawned.branch.created_source_branch);
}

#[test]
fn a_branch_with_unique_commits_is_kept() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let head = spawn_with_commit(&repo, "feature-a");

    let outcome = remove(&repo, "feature-a", SourceBranchPolicy::Auto).expect("remove");
    assert_eq!(
        outcome.source_branch,
        SourceBranchOutcome::Kept(KeptBranch::Unique { tip: head.clone() })
    );
    assert_eq!(branch_rev(&repo, "feature-a"), Some(head));
}

#[test]
fn a_merged_branch_is_deleted() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let head = spawn_with_commit(&repo, "feature-a");
    git(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge", "feature-a"],
    );

    let outcome = remove(&repo, "feature-a", SourceBranchPolicy::Auto).expect("remove");
    assert_eq!(
        outcome.source_branch,
        SourceBranchOutcome::Deleted {
            tip: head,
            found_on: Some("`main`".to_owned()),
        }
    );
    assert_eq!(branch_rev(&repo, "feature-a"), None);
}

#[test]
fn a_branch_on_a_remote_tracking_ref_is_deleted() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let head = spawn_with_commit(&repo, "feature-a");
    git(
        &repo,
        &["update-ref", "refs/remotes/origin/feature-a", &head],
    );

    let outcome = remove(&repo, "feature-a", SourceBranchPolicy::Auto).expect("remove");
    assert_eq!(
        outcome.source_branch,
        SourceBranchOutcome::Deleted {
            tip: head,
            found_on: Some("`origin/feature-a`".to_owned()),
        }
    );
}

#[test]
fn a_forced_delete_leaves_the_commits_pinned_by_checkpoints() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let head = spawn_with_commit(&repo, "feature-a");

    let outcome = remove(&repo, "feature-a", SourceBranchPolicy::Delete).expect("remove");
    assert_eq!(
        outcome.source_branch,
        SourceBranchOutcome::Deleted {
            tip: head.clone(),
            found_on: None,
        }
    );
    assert_eq!(branch_rev(&repo, "feature-a"), None);
    let pinning = git(
        &repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "--contains",
            &head,
            "refs/newgit/checkpoints",
        ],
    );
    assert!(
        !pinning.is_empty(),
        "the kept checkpoint still roots the deleted branch's commits"
    );
}

#[test]
fn keep_branch_keeps_even_a_branch_with_nothing_unique() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    manager(&repo).spawn("smoke", None).expect("spawn");

    let outcome = remove(&repo, "smoke", SourceBranchPolicy::Keep).expect("remove");
    assert_eq!(
        outcome.source_branch,
        SourceBranchOutcome::Kept(KeptBranch::Asked)
    );
    assert!(branch_rev(&repo, "smoke").is_some());
}

#[test]
fn an_adopted_branch_is_never_deleted_unasked() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    git(&repo, &["branch", "develop"]);
    let spawned = manager(&repo).spawn("develop", None).expect("spawn");
    assert!(!spawned.created_source_branch);

    let outcome = remove(&repo, "develop", SourceBranchPolicy::Auto).expect("remove");
    assert_eq!(
        outcome.source_branch,
        SourceBranchOutcome::Kept(KeptBranch::Adopted)
    );
    assert!(branch_rev(&repo, "develop").is_some());

    // Asking explicitly still works.
    manager(&repo).spawn("develop", None).expect("respawn");
    let outcome = remove(&repo, "develop", SourceBranchPolicy::Delete).expect("remove");
    assert!(matches!(
        outcome.source_branch,
        SourceBranchOutcome::Deleted { .. }
    ));
    assert_eq!(branch_rev(&repo, "develop"), None);
}

#[test]
fn a_branch_checked_out_in_the_store_is_kept_and_a_forced_delete_refuses_up_front() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let spawned = manager(&repo).spawn("smoke", None).expect("spawn");
    git(&repo, &["switch", "-q", "smoke"]);

    let error = remove(&repo, "smoke", SourceBranchPolicy::Delete)
        .expect_err("cannot delete a checked-out branch")
        .to_string();
    assert!(error.contains("checked out"), "{error}");
    assert!(
        spawned.branch.workspace_path.is_dir(),
        "the refusal comes before anything is torn down"
    );
    MetadataStore::at(repo.clone())
        .find_branch("smoke")
        .expect("still an instance");

    let outcome = remove(&repo, "smoke", SourceBranchPolicy::Auto).expect("remove");
    assert_eq!(
        outcome.source_branch,
        SourceBranchOutcome::Kept(KeptBranch::CheckedOut(repo.clone()))
    );
    assert!(branch_rev(&repo, "smoke").is_some());
}

#[test]
fn a_branch_already_gone_is_reported_not_an_error() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    manager(&repo).spawn("smoke", None).expect("spawn");
    git(&repo, &["branch", "-D", "smoke"]);

    let outcome = remove(&repo, "smoke", SourceBranchPolicy::Delete).expect("remove");
    assert_eq!(outcome.source_branch, SourceBranchOutcome::Missing);
}
