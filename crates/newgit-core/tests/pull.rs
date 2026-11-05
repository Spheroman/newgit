//! `newgit pull <tracker>` (docs/design/trackers.md, *Pulling a child is
//! explicit*): a mounted tracker's `main` moved elsewhere (an instance rooted
//! at it merged, an upstream synced); a spawn says so and writes nothing;
//! the pull is one view commit that brings the new commits in as a merge,
//! and it merges into `main` like any instance commit.

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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A project with `lib` carved at `libs/lib`, and two commits on `lib`'s
/// `main` made elsewhere (its own files, at its own root). Returns the store
/// and the commit `lib`'s `main` is at now.
fn setup(temp: &Utf8Path) -> (MetadataStore, String) {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("libs/lib")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("libs/lib/index.js"), "one\n").expect("write");
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
    BranchManager::open(store.clone())
        .expect("manager")
        .create_git_tracker("lib", "project-devs", &[Utf8PathBuf::from("libs/lib")])
        .expect("carve");
    let manager = BranchManager::open(store.clone()).expect("reopen");
    manager.merge("main", "main").expect("publish");

    // Elsewhere: `lib` alone, two commits on its `main`.
    let tracker_store = store.paths().tracker_store.clone();
    let elsewhere = temp.join("lib");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    git(&elsewhere, &["init", "-q", "-b", "main"]);
    git(&elsewhere, &["config", "commit.gpgSign", "false"]);
    git(
        &elsewhere,
        &[
            "fetch",
            "-q",
            tracker_store.as_str(),
            "+refs/trackers/lib/main:refs/remotes/store/main",
        ],
    );
    git(
        &elsewhere,
        &["reset", "-q", "--hard", "refs/remotes/store/main"],
    );
    for (file, message) in [("a.js", "lib: a"), ("b.js", "lib: b")] {
        std::fs::write(elsewhere.join(file), "x\n").expect("write");
        git(&elsewhere, &["add", "-A"]);
        git(&elsewhere, &["commit", "-q", "-m", message]);
    }
    let tip = git(&elsewhere, &["rev-parse", "HEAD"]);
    git(
        &elsewhere,
        &[
            "push",
            "-q",
            tracker_store.as_str(),
            "HEAD:refs/trackers/lib/main",
        ],
    );
    (store, tip)
}

#[test]
fn a_spawn_says_a_mounted_tracker_is_behind_and_the_pull_brings_it_in_as_a_merge() {
    let (_guard, temp) = tempdir();
    let (store, tip) = setup(&temp);
    let repo = store.paths().project_root.clone();
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let manager = BranchManager::open(store.clone()).expect("manager");

    let spawned = manager.spawn("look", None).expect("spawn");
    assert_eq!(
        spawned.behind,
        vec![("lib".to_owned(), 2, "main".to_owned())],
        "a spawn says so"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        before,
        "and writes nothing"
    );

    let pulled = manager.pull(None, "lib").expect("pull");
    assert_eq!(pulled.commits, 2);
    let view = pulled.view.expect("a view commit");
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        view,
        "the project root takes it"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Pull lib: 2 commits from main"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD^@"]),
        before,
        "one commit on one parent"
    );
    assert_eq!(
        git(&repo, &["ls-tree", "--name-only", "HEAD", "libs/lib/"]),
        "libs/lib/a.js\nlibs/lib/b.js\nlibs/lib/index.js"
    );
    assert_eq!(
        git(
            &repo,
            &["log", "--format=%s", "HEAD", "--", "libs/lib/b.js"]
        ),
        "Pull lib: 2 commits from main",
        "`git log` shows each pull as one commit"
    );
    // Root pins what it pulled, and a second pull has nothing to do.
    let tracker_store = store.paths().tracker_store.clone();
    let pinned = git(
        &repo,
        &[
            &format!("--git-dir={tracker_store}"),
            "rev-parse",
            "refs/trackers/root/instances/main:.newgit/pins/lib",
        ],
    );
    assert_eq!(pinned, tip);
    assert_eq!(manager.pull(None, "lib").expect("again").commits, 0);

    // It merges into `main` like any commit.
    manager.merge("main", "main").expect("merge");
    let main = git(
        &repo,
        &[
            &format!("--git-dir={tracker_store}"),
            "rev-parse",
            "refs/trackers/root/main:.newgit/pins/lib",
        ],
    );
    assert_eq!(main, tip);
    let spawned = manager.spawn("after", None).expect("spawn");
    assert!(spawned.behind.is_empty(), "{:?}", spawned.behind);
}

#[test]
fn a_pull_inside_an_instance_fast_forwards_its_workspace() {
    let (_guard, temp) = tempdir();
    let (store, _) = setup(&temp);
    let manager = BranchManager::open(store.clone()).expect("manager");
    let workspace = manager
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    let pulled = manager.pull(Some("feature"), "lib").expect("pull");
    assert_eq!(
        git(&workspace, &["rev-parse", "HEAD"]),
        pulled.view.expect("view")
    );
    assert!(workspace.join("libs/lib/b.js").is_file());
}
