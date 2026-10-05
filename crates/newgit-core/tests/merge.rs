//! `newgit merge` (newgit-trackers.md, step 5), on one machine: an instance's
//! committed work into `main`, tracker by tracker — a fast-forward where
//! `main` has not moved, a real merge where both changed a tracker, and
//! nothing at all when anything conflicts.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, TrackerMergeAction};
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

fn work(manager: &BranchManager, instance: &str, file: &str, contents: &str) {
    manager.spawn(instance, None).expect("spawn");
    let workspace = manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .workspace_path;
    std::fs::write(workspace.join(file), contents).expect("write");
    git(&workspace, &["add", "-A"]);
    git(
        &workspace,
        &["commit", "-q", "-m", &format!("{instance}: {file}")],
    );
}

fn root_main(manager: &BranchManager) -> String {
    git(
        &manager.store().paths().tracker_store,
        &["rev-parse", "refs/trackers/root/main"],
    )
}

fn root_files(manager: &BranchManager) -> String {
    git(
        &manager.store().paths().tracker_store,
        &["ls-tree", "-r", "--name-only", "refs/trackers/root/main"],
    )
}

#[test]
fn work_onto_an_unmoved_main_fast_forwards_just_the_trackers_it_changed() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    work(&manager, "feature-a", "a.txt", "a\n");
    let outcome = manager.merge("feature-a", "main").expect("merge");
    let root = outcome
        .trackers
        .iter()
        .find(|merge| merge.tracker == "root")
        .expect("root");
    assert_eq!(root.action, TrackerMergeAction::FastForward);
    assert_eq!(root_main(&manager), root.commit);
    assert!(root_files(&manager).contains("a.txt"));
    assert!(
        outcome
            .trackers
            .iter()
            .all(|merge| merge.tracker == "root" || merge.action == TrackerMergeAction::Unchanged),
        "{:?}",
        outcome.trackers
    );
}

#[test]
fn two_instances_that_changed_one_tracker_merge_for_real() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    work(&manager, "feature-b", "b.txt", "b\n");
    work(&manager, "feature-c", "c.txt", "c\n");
    manager.merge("feature-b", "main").expect("first merge");
    let outcome = manager.merge("feature-c", "main").expect("second merge");
    let root = outcome
        .trackers
        .iter()
        .find(|merge| merge.tracker == "root")
        .expect("root");
    assert_eq!(root.action, TrackerMergeAction::Merged);
    let files = root_files(&manager);
    assert!(
        files.contains("b.txt") && files.contains("c.txt"),
        "{files}"
    );
    let parents = git(
        &manager.store().paths().tracker_store,
        &[
            "rev-list",
            "--parents",
            "-n",
            "1",
            "refs/trackers/root/main",
        ],
    );
    assert_eq!(parents.split(' ').count(), 3, "a merge commit: {parents}");
}

#[test]
fn a_conflict_moves_nothing_and_names_the_tracker_and_file() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    work(&manager, "feature-d", "README.md", "d's\n");
    work(&manager, "feature-e", "README.md", "e's\n");
    manager.merge("feature-d", "main").expect("first merge");
    let before = root_main(&manager);
    let error = manager
        .merge("feature-e", "main")
        .expect_err("conflict")
        .to_string();
    assert!(
        error.contains("`root`") && error.contains("README.md"),
        "{error}"
    );
    assert_eq!(root_main(&manager), before, "nothing moved");
}
