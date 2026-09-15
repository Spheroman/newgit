use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
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

/// A store repo with one commit, plus an initialized metadata store whose
/// workspace root is redirected inside the tempdir.
fn setup(temp: &Utf8Path) -> MetadataStore {
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
    store
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn manager(store: MetadataStore) -> BranchManager {
    BranchManager::open(store).expect("manager")
}

/// Spawning with no `--from` records the branch `HEAD` pointed at (`main`)
/// as the base — not the literal string `"HEAD"` — so a later `status` has
/// something to compare against and report.
#[test]
fn spawn_records_base_from_head() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = manager(store);

    let outcome = m.spawn("feature-a", None).expect("spawn");
    assert_eq!(outcome.branch.base_ref.as_deref(), Some("main"));
    assert!(outcome.branch.base_rev.is_some());
}

/// `spawn --from <branch>` records that branch by name.
#[test]
fn spawn_records_explicit_from_branch() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    git(&root, &["checkout", "-q", "-b", "develop"]);
    git(&root, &["checkout", "-q", "main"]);
    let m = manager(store);

    let outcome = m.spawn("feature-a", Some("develop")).expect("spawn");
    assert_eq!(outcome.branch.base_ref.as_deref(), Some("develop"));
}

/// When the source branch already existed, `spawn` never chose a base for
/// it — there is nothing recorded, and `status` reports no drift rather
/// than guessing one.
#[test]
fn spawn_onto_existing_branch_records_no_base() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    git(&root, &["branch", "feature-a"]);
    let m = manager(store);

    let outcome = m.spawn("feature-a", None).expect("spawn");
    assert!(!outcome.created_source_branch);
    assert_eq!(outcome.branch.base_ref, None);
    assert_eq!(outcome.branch.base_rev, None);

    let reports = m.statuses().expect("statuses");
    let report = reports
        .iter()
        .find(|r| r.branch.name == "feature-a")
        .expect("report");
    assert!(report.base.is_none());
}

/// The headline case from #38: once `main` gains commits an instance never
/// pulled in, `status` reports how many — recomputed fresh from the store's
/// own refs, not cached from spawn time.
#[test]
fn status_reports_commits_the_base_gained() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let m = manager(store);

    m.spawn("feature-a", None).expect("spawn");

    // Two more commits land on `main` after the instance branched.
    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-q", "-m", "a"]);
    std::fs::write(root.join("b.txt"), "b\n").expect("write");
    git(&root, &["add", "b.txt"]);
    git(&root, &["commit", "-q", "-m", "b"]);

    let reports = m.statuses().expect("statuses");
    let report = reports
        .iter()
        .find(|r| r.branch.name == "feature-a")
        .expect("report");
    let base = report.base.as_ref().expect("base recorded");
    assert_eq!(base.base_ref, "main");
    assert_eq!(base.ahead, 2);
}

/// No drift when nothing has moved since spawn.
#[test]
fn status_reports_no_drift_when_base_unchanged() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = manager(store);

    m.spawn("feature-a", None).expect("spawn");

    let reports = m.statuses().expect("statuses");
    let report = reports
        .iter()
        .find(|r| r.branch.name == "feature-a")
        .expect("report");
    assert_eq!(report.base.as_ref().expect("base recorded").ahead, 0);
}

/// A base branch deleted out from under an instance is not silently treated
/// as "no drift" — `status` has nothing left to compare against, so it
/// reports nothing rather than guessing.
#[test]
fn status_reports_no_base_when_base_branch_deleted() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let m = manager(store);

    git(&root, &["checkout", "-q", "-b", "topic"]);
    git(&root, &["checkout", "-q", "main"]);
    let outcome = m.spawn("feature-a", Some("topic")).expect("spawn");
    assert_eq!(outcome.branch.base_ref.as_deref(), Some("topic"));
    git(&root, &["branch", "-D", "topic"]);

    let reports = m.statuses().expect("statuses");
    let report = reports
        .iter()
        .find(|r| r.branch.name == "feature-a")
        .expect("report");
    assert!(report.base.is_none());
}
