//! Readiness (from setting up a real project with only the help text): an
//! instance says whether it works — checked, not remembered — a checkpoint
//! goes ahead, degraded, past a resource that never came up, and undo
//! rebuilds a tree that is gone even when its inputs did not move.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, UndoOptions};
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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

/// An install: builds `node_modules` from the inputs `[identity]` names.
const DEPS: &str = r#"ownership = "workspace"

[identity]
paths = ["README.md"]
produces = ["node_modules"]

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/marker"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#;

/// A resource that never comes up, and whose capture fails too.
const BROKEN: &str = r#"ownership = "branch"

[actions.prepare]
command = "echo the database would not start; exit 1"

[checkpoint]
mode = "command"
command = "exit 1"
"#;

fn workspace(manager: &BranchManager, instance: &str) -> Utf8PathBuf {
    manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .workspace_path
}

#[test]
fn an_instance_says_whether_it_works_checked_not_remembered() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", DEPS);
    let manager = manager(store);
    manager.spawn("feature-a", None).expect("spawn");
    let branch = manager.store().find_branch("feature-a").expect("branch");
    assert!(manager.readiness(&branch).is_ready());

    std::fs::remove_dir_all(workspace(&manager, "feature-a").join("node_modules")).expect("rm");
    let readiness = manager.readiness(&branch);
    assert!(!readiness.is_ready());
    assert!(
        readiness
            .problems
            .iter()
            .any(|problem| problem.contains("node_modules")),
        "{:?}",
        readiness.problems
    );
}

#[test]
fn undo_rebuilds_a_tree_that_is_gone_even_when_its_inputs_did_not_move() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", DEPS);
    let manager = manager(store);
    manager.spawn("feature-a", None).expect("spawn");
    manager.checkpoint("feature-a", None).expect("checkpoint");
    let tree = workspace(&manager, "feature-a").join("node_modules");
    std::fs::remove_dir_all(&tree).expect("rm");
    manager
        .undo("feature-a", &UndoOptions::default())
        .expect("undo");
    assert!(tree.join("marker").exists(), "rebuilt, not skipped");
}

#[test]
fn a_checkpoint_goes_ahead_degraded_past_a_resource_that_never_came_up() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", DEPS);
    write_resource(&store, "broken", BROKEN);
    let manager = manager(store);
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    assert_eq!(spawned.failed_resources(), vec!["broken"]);
    let branch = manager.store().find_branch("feature-a").expect("branch");
    let readiness = manager.readiness(&branch);
    assert!(
        readiness
            .problems
            .iter()
            .any(|problem| problem.contains("`broken` failed"))
    );

    let checkpoint = manager
        .checkpoint("feature-a", None)
        .expect("the checkpoint goes ahead");
    assert_eq!(checkpoint.degraded, vec!["broken"]);
    let deps = checkpoint
        .record
        .resource_states
        .iter()
        .find(|state| state.name == "deps")
        .expect("deps");
    assert!(
        deps.state_ref.is_some(),
        "everything else is captured as usual"
    );
}
