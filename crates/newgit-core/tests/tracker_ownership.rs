//! A path belongs to its most specific claimant: trackers nest, and the root
//! tracker owns whatever nobody else claims (docs/design/trackers.md, step 2).

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
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
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
}

fn define(store: &MetadataStore, name: &str, path: &str) {
    std::fs::write(
        store.paths().trackers.join(format!("{name}.toml")),
        format!("audience = \"project-devs\"\nstorage = \"local\"\npaths = [\"{path}\"]\n"),
    )
    .expect("write definition");
}

#[test]
fn init_gives_root_its_own_copy_of_the_project_history() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    assert!(
        store.paths().trackers.join("root.toml").is_file(),
        "init writes the root tracker"
    );
    let manager = BranchManager::open(store.clone()).expect("manager");
    let root = manager.root_tracker();
    assert!(root.owns_everything() && !root.secret);
    assert!(
        manager
            .tracker_definitions()
            .iter()
            .all(|definition| !definition.owns_everything())
    );

    let trackers = store.paths().tracker_store.clone();
    assert_eq!(
        git(&trackers, &["rev-parse", "refs/trackers/root/main"]),
        git(&repo, &["rev-parse", "HEAD"]),
        "root's main is the project's current commit"
    );
    assert_eq!(
        git(
            &trackers,
            &["log", "--format=%s", "refs/trackers/root/main"]
        ),
        "initial"
    );
    // A real copy, not borrowed objects: Git's cleanup in the project repo
    // must not be able to take root's history with it.
    assert!(!trackers.join("objects/info/alternates").exists());
    std::fs::rename(repo.join(".git"), repo.join(".git-away")).expect("hide the project repo");
    assert_eq!(
        git(
            &trackers,
            &["log", "--format=%s", "refs/trackers/root/main"]
        ),
        "initial"
    );
    std::fs::rename(repo.join(".git-away"), repo.join(".git")).expect("restore");
}

#[test]
fn root_is_committed_with_git_not_captured() {
    let (_guard, temp) = tempdir();
    let manager = BranchManager::open(setup(&temp)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");
    let error = manager
        .capture_tracker("feature-a", "root")
        .expect_err("root is not captured");
    assert!(
        error.to_string().contains("commit to it with Git"),
        "{error}"
    );
    assert!(manager.remove_tracker("root").is_err());
}

#[test]
fn a_nested_tracker_keeps_its_files_out_of_the_outer_one() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    define(&store, "generated", "src/generated");
    define(&store, "sdk", "src/generated/sdk");
    let manager = BranchManager::open(store).expect("nesting is allowed");
    let workspace = manager
        .spawn("feature-a", None)
        .expect("spawn")
        .branch
        .workspace_path;

    std::fs::create_dir_all(workspace.join("src/generated/sdk")).expect("mkdir");
    std::fs::write(workspace.join("src/generated/types.ts"), "types v1\n").expect("write");
    std::fs::write(workspace.join("src/generated/sdk/client.ts"), "client v1\n").expect("write");

    let outer = manager
        .capture_tracker("feature-a", "generated")
        .expect("capture outer");
    assert_eq!(
        outer.files, 1,
        "the outer tracker captures only what it owns"
    );
    let inner = manager
        .capture_tracker("feature-a", "sdk")
        .expect("capture inner");
    assert_eq!(inner.files, 1);

    // Restoring the outer tracker neither deletes nor overwrites the inner one's file.
    std::fs::write(workspace.join("src/generated/types.ts"), "types v2\n").expect("write");
    std::fs::write(workspace.join("src/generated/sdk/client.ts"), "client v2\n").expect("write");
    manager
        .checkout_tracker("feature-a", "generated", Some(&outer.rev))
        .expect("checkout outer");
    assert_eq!(
        std::fs::read_to_string(workspace.join("src/generated/types.ts")).expect("read"),
        "types v1\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("src/generated/sdk/client.ts")).expect("read"),
        "client v2\n",
        "the inner tracker's file is its own"
    );
}
