//! On macOS a workspace is cloned from a template checkout rather than
//! `git clone`d (#235). It has to come out the same as the clone would.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::ArchivedCheckpoints;
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

/// The workspace is on its own branch, tracking the store's, with no other
/// local branch; it starts clean, and an edit made straight away is seen.
/// The edit keeps the file's size and lands in the same second the file was
/// checked out, which is the case `core.checkStat=minimal` alone would miss.
#[test]
fn a_templated_workspace_is_the_clone_it_replaces() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let m = BranchManager::open(store).expect("manager");

    let outcome = m.spawn("feature-a", None).expect("spawn");
    let workspace = outcome.branch.workspace_path.clone();
    if cfg!(target_os = "macos") {
        assert!(
            temp.join("workspaces/.source-template/.git").is_dir(),
            "the spawn went through the template"
        );
        assert_eq!(git(&workspace, &["config", "core.checkStat"]), "minimal");
    }
    assert_eq!(outcome.template_warning, None);
    assert!(!workspace.join(".git/FETCH_HEAD").exists());

    assert_eq!(
        git(&workspace, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature-a"
    );
    assert_eq!(
        git(&workspace, &["rev-parse", "HEAD"]),
        git(&root, &["rev-parse", "feature-a"])
    );
    assert_eq!(
        git(
            &workspace,
            &["for-each-ref", "--format=%(refname)", "refs/heads"]
        ),
        "refs/heads/feature-a"
    );
    assert_eq!(
        git(&workspace, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feature-a"
    );
    assert_eq!(git(&workspace, &["status", "--porcelain"]), "");

    std::fs::write(workspace.join("README.md"), "jello\n").expect("edit");
    assert_eq!(
        git(&workspace, &["status", "--porcelain"]),
        "M README.md",
        "a same-size edit in the same second is still a change"
    );
}

/// The template follows the store: a commit made after the first spawn is
/// in the next one, along with the store's new branch.
#[test]
fn the_next_spawn_sees_what_the_store_gained_since() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let m = BranchManager::open(store).expect("manager");
    m.spawn("first", None).expect("first spawn");

    std::fs::write(root.join("NEW.md"), "new\n").expect("write");
    git(&root, &["add", "NEW.md"]);
    git(&root, &["commit", "-q", "-m", "second"]);

    let second = m.spawn("second", None).expect("second spawn");
    let workspace = second.branch.workspace_path;
    assert_eq!(
        std::fs::read_to_string(workspace.join("NEW.md")).expect("new file"),
        "new\n"
    );
    assert_eq!(
        git(&workspace, &["rev-parse", "HEAD"]),
        git(&root, &["rev-parse", "main"])
    );
    assert_eq!(git(&workspace, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&workspace, &["rev-parse", "origin/second"]),
        git(&root, &["rev-parse", "main"])
    );
}

/// The template lives under the workspace root, and `cleanup` must not
/// take it for an orphaned workspace.
#[test]
fn cleanup_leaves_the_template_alone() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = BranchManager::open(store).expect("manager");
    m.spawn("feature-a", None).expect("spawn");

    let outcome = m.cleanup(true, ArchivedCheckpoints::Keep).expect("cleanup");
    assert!(
        outcome.orphan_workspaces.is_empty(),
        "{:?}",
        outcome.orphan_workspaces
    );
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
}

/// A template that cannot be used still gives a workspace, through `git
/// clone`, and the spawn says why instead of quietly paying for both.
#[cfg(target_os = "macos")]
#[test]
fn a_failing_template_falls_back_and_says_so() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = BranchManager::open(store).expect("manager");
    // A file where the template directory belongs: building it fails.
    std::fs::create_dir_all(temp.join("workspaces")).expect("mkdir");
    std::fs::write(temp.join("workspaces/.source-template"), "").expect("write");

    let outcome = m.spawn("feature-a", None).expect("spawn");
    assert!(
        outcome.template_warning.is_some(),
        "the fallback is reported"
    );
    let workspace = outcome.branch.workspace_path;
    assert_eq!(
        git(&workspace, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature-a"
    );
    assert_eq!(git(&workspace, &["status", "--porcelain"]), "");
    let check_stat = Command::new("git")
        .args([
            "-C",
            workspace.as_str(),
            "config",
            "--local",
            "core.checkStat",
        ])
        .output()
        .expect("git runs");
    assert!(
        !check_stat.status.success(),
        "a cloned workspace keeps Git's defaults"
    );
}
