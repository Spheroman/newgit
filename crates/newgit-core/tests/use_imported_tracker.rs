//! `newgit tracker use` on an instance forked before a tracker was imported
//! (newgit-trackers.md, *A view without the anchor leaves an imported
//! tracker out*): there is nothing of it to check out, so it says to merge
//! `main` first rather than binding an empty path.

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

#[test]
fn using_an_imported_tracker_before_its_import_says_to_merge_main() {
    let temp = tempfile::tempdir().expect("tempdir");
    let temp = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
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
    let anchor = git(&repo, &["rev-parse", "HEAD"]);

    // `t3code` as an import leaves it: its own paths, anchored on the
    // project root, with history this instance's view does not have.
    BranchManager::open(store.clone())
        .expect("manager")
        .create_code_tracker(
            "t3code",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code")],
        )
        .expect("tracker");
    let definition = repo.join(".newgit/trackers/t3code.toml");
    let mut contents = std::fs::read_to_string(&definition).expect("definition");
    contents.push_str(&format!(
        "imported = {{ anchor = \"{anchor}\", tip = \"{anchor}\" }}\n"
    ));
    std::fs::write(&definition, contents).expect("write definition");
    let manager = BranchManager::open(store).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");
    let mut branch = manager.store().find_branch("feature-a").expect("instance");
    branch.dropped.insert("t3code".to_owned());
    manager.store().save_branch_record(&branch).expect("save");

    let error = manager
        .use_tracker("feature-a", "t3code")
        .expect_err("nothing of t3code is in this instance");
    let message = error.to_string();
    assert!(
        message.contains("imported after `feature-a` forked"),
        "{message}"
    );
    assert!(message.contains("merge `main` into it first"), "{message}");
    let branch = manager.store().find_branch("feature-a").expect("instance");
    assert!(branch.dropped.contains("t3code"), "still dropped");
}
