//! Trackers in Git: committed content with its own history, at its own root.
//! Carving is a commit (docs/design/trackers.md, *Carving is a commit*), so
//! composing still reproduces the project's history exactly.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::compose;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

fn git_env(dir: &Utf8Path, args: &[&str], env: &[(&str, String)]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    git_env(dir, args, &[])
}

fn commit(repo: &Utf8Path, files: &[(&str, &str)], message: &str, time: i64) {
    for (path, contents) in files {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
    }
    git(repo, &["add", "-A"]);
    let who = |key: &'static str, value: String| (key, value);
    git_env(
        repo,
        &["commit", "-q", "--no-gpg-sign", "-m", message],
        &[
            who("GIT_AUTHOR_NAME", "ana".to_owned()),
            who("GIT_AUTHOR_EMAIL", "ana@example.com".to_owned()),
            who("GIT_AUTHOR_DATE", format!("@{time} +0000")),
            who("GIT_COMMITTER_NAME", "ana".to_owned()),
            who("GIT_COMMITTER_EMAIL", "ana@example.com".to_owned()),
            who("GIT_COMMITTER_DATE", format!("@{time} +0000")),
        ],
    );
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A project with app code and an Electron app, one commit touching both.
fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(
        &repo,
        &[("README.md", "hello\n"), ("apps/electron/main.js", "v1\n")],
        "initial",
        1_000,
    );
    commit(&repo, &[("README.md", "docs\n")], "docs", 2_000);
    commit(
        &repo,
        &[("README.md", "both\n"), ("apps/electron/main.js", "v2\n")],
        "both at once",
        3_000,
    );
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
}

#[test]
fn init_mounts_access_at_the_definitions_and_root_holds_the_rest_of_newgit() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    assert!(!store.paths().trackers.join("project.toml").exists());
    let manager = BranchManager::open(store).expect("manager");
    assert!(
        manager
            .carved_trackers()
            .iter()
            .all(|definition| definition.name != "project"),
        "`project` folds into root"
    );
    let access = manager
        .carved_trackers()
        .iter()
        .find(|definition| definition.name == "access")
        .expect("access");
    assert_eq!(access.mount, Utf8PathBuf::from(".newgit/trackers"));
    assert!(
        manager
            .root_tracker()
            .owns(camino::Utf8Path::new(".newgit/config.toml")),
        "root owns .newgit"
    );
}

#[test]
fn carving_is_one_commit_and_the_view_keeps_every_id() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let manager = BranchManager::open(store.clone()).expect("manager");
    let outcome = manager
        .create_git_tracker(
            "electron",
            "project-devs",
            &[Utf8PathBuf::from("apps/electron")],
        )
        .expect("carve");
    assert_eq!(outcome.source, "root");
    assert_eq!(outcome.commits, 1, "one first commit, holding the files");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD^"]),
        before,
        "one commit on top"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Create tracker `electron` at apps/electron"
    );

    // The tracker's history starts at the carve, its files at its root.
    let trackers = store.paths().tracker_store.clone();
    let electron = "refs/trackers/electron/instances/main";
    assert_eq!(
        git(&trackers, &["ls-tree", "-r", "--name-only", electron]),
        "main.js"
    );
    assert_eq!(git(&trackers, &["rev-list", "--count", electron]), "1");
    // Root's commit removes the files and pins it.
    let root = git(
        &trackers,
        &["rev-parse", "refs/trackers/root/instances/main"],
    );
    let root_files = git(&trackers, &["ls-tree", "-r", "--name-only", &root]);
    assert!(!root_files.contains("apps/electron"), "{root_files}");
    assert!(root_files.contains(".newgit/pins/electron"), "{root_files}");

    // Composing root alone, anywhere, reproduces the project's commits.
    let manager = BranchManager::open(store.clone()).expect("reopen");
    assert!(
        manager
            .root_tracker()
            .excluded
            .contains(&Utf8PathBuf::from("apps/electron"))
    );
    let everything = |_: &str| true;
    let view = compose::compose(
        &repo,
        "root",
        &[("refs/newgit/views/check".to_owned(), root)],
        &compose::ComposeOptions {
            readable: &everything,
            admit: None,
        },
    )
    .expect("compose")
    .remove(0);
    assert_eq!(view.tip.as_deref(), Some(head.as_str()));
}

#[test]
fn a_git_tracker_is_committed_to_not_captured() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager
        .create_git_tracker(
            "electron",
            "project-devs",
            &[Utf8PathBuf::from("apps/electron")],
        )
        .expect("carve");
    let manager = BranchManager::open(store).expect("reopen");
    manager.spawn("feature-a", None).expect("spawn");
    let capture = manager
        .capture_tracker("feature-a", "electron")
        .expect_err("not captured");
    assert!(capture.to_string().contains("tracker in Git"), "{capture}");
    let track = manager
        .track_paths("electron", &[Utf8PathBuf::from("apps/desktop")])
        .expect_err("paths are set at creation");
    assert!(track.to_string().contains("tracker in Git"), "{track}");
}
