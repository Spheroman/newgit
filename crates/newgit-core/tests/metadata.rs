use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::branch::InstanceStatus;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::{NewgitError, SourceSubstrate};

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

#[test]
fn init_is_not_repeatable_and_writes_gitignore() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);

    let gitignore = store.paths().metadata_root.join(".gitignore");
    let contents = std::fs::read_to_string(gitignore).expect("gitignore written");
    for line in ["/local/", "/branches/", "/snapshots/", "/logs/", "/state/"] {
        assert!(contents.contains(line), "missing {line}");
    }

    let again = MetadataStore::init(&store.paths().project_root, "proj", SourceSubstrate::Git);
    assert!(matches!(again, Err(NewgitError::AlreadyExists(_))));
}

#[test]
fn spawn_creates_clone_record_and_marker() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = BranchManager::open(store).expect("manager");

    let outcome = manager.spawn("feature-a", None).expect("spawn");
    assert!(outcome.created_source_branch);
    let branch = &outcome.branch;
    assert_eq!(branch.status, InstanceStatus::Active);
    assert_eq!(branch.workspace_path, temp.join("workspaces/feature-a"));

    // A full real clone: .git is a directory, the branch is checked out.
    assert!(branch.workspace_path.join(".git").is_dir());
    assert!(branch.workspace_path.join("README.md").is_file());
    assert!(outcome.record_path.is_file());
    assert!(
        branch
            .workspace_path
            .join(".newgit/local/instance.toml")
            .is_file()
    );

    // Discovery from inside the workspace resolves to the store and infers
    // the current instance.
    let context = MetadataStore::discover(&branch.workspace_path).expect("discover");
    assert_eq!(context.current_branch.as_deref(), Some("feature-a"));
    assert_eq!(
        context.store.paths().project_root,
        manager.store().paths().project_root
    );
}

#[test]
fn spawn_rejects_slug_collisions() {
    let (_guard, temp) = tempdir();
    let manager = BranchManager::open(setup(&temp)).expect("manager");

    manager.spawn("feature-a", None).expect("first spawn");
    let collision = manager.spawn("Feature/A", None);
    assert!(matches!(
        collision,
        Err(NewgitError::BranchInstanceExists { .. })
    ));
}

#[test]
fn two_instances_then_remove_one() {
    let (_guard, temp) = tempdir();
    let manager = BranchManager::open(setup(&temp)).expect("manager");

    let a = manager.spawn("feature-a", None).expect("spawn a");
    manager.spawn("feature-b", None).expect("spawn b");
    assert_eq!(manager.statuses().expect("statuses").len(), 2);

    let removed = manager
        .remove("feature-a", &temp, ArchivedCheckpoints::Keep)
        .expect("remove");
    assert!(!a.branch.workspace_path.exists());
    assert!(removed.archived_record.is_file());

    let reports = manager.statuses().expect("statuses");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].branch.name, "feature-b");
    assert!(reports[0].workspace_exists);
    assert!(reports[0].live_rev.is_some());
}

#[test]
fn remove_refuses_from_inside_the_workspace() {
    let (_guard, temp) = tempdir();
    let manager = BranchManager::open(setup(&temp)).expect("manager");

    let outcome = manager.spawn("feature-a", None).expect("spawn");
    let inside = outcome.branch.workspace_path.clone();
    assert!(matches!(
        manager.remove("feature-a", &inside, ArchivedCheckpoints::Keep),
        Err(NewgitError::Unsupported(_))
    ));
    assert!(inside.exists());
}

#[test]
fn action_log_paths_do_not_collide_within_one_second() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);

    let first = store.action_log_path("feature-a", "run");
    let second = store.action_log_path("feature-a", "run");

    assert_ne!(first, second);
    assert_eq!(first.parent(), second.parent());
}
