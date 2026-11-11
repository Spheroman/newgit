//! `newgit pull` with no tracker (#345): every tracker in Git root mounts
//! moves its pin to its `follow` ref in one commit, on the branch it ran
//! on. Nothing moved, no commit. A pin holding commits its ref lacks, and a
//! tracker outside Git, are left alone.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;
use newgit_core::tracker_repo::TrackerRepo;

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

const ENV: &str = "API_URL=http://127.0.0.1:4000\n";

/// A project with `lib` and `ui` carved at `libs/lib` and `libs/ui`, both
/// published on `main`, and `env`, a tracker outside Git owning
/// `.env.local`.
fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("libs/lib")).expect("mkdir");
    std::fs::create_dir_all(repo.join("libs/ui")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("libs/lib/index.js"), "one\n").expect("write");
    std::fs::write(repo.join("libs/ui/index.js"), "one\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");

    std::fs::write(repo.join(".env.local"), ENV).expect("write env");
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager
        .create_tracker("env", "user", Storage::Local, false)
        .expect("create env");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track env");
    manager.seed_tracker_from_store("env").expect("seed env");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);

    for (tracker, mount) in [("lib", "libs/lib"), ("ui", "libs/ui")] {
        BranchManager::open(store.clone())
            .expect("manager")
            .create_git_tracker(tracker, "project-devs", &[Utf8PathBuf::from(mount)])
            .expect("carve");
    }
    BranchManager::open(store.clone())
        .expect("reopen")
        .merge("main", "main")
        .expect("publish");
    store
}

/// Elsewhere: `tracker` alone, one commit per file on its `main`. Returns
/// the commit `main` is at now.
fn advance(temp: &Utf8Path, store: &MetadataStore, tracker: &str, files: &[&str]) -> String {
    let tracker_store = store.paths().tracker_store.clone();
    let elsewhere = temp.join(format!("elsewhere-{tracker}"));
    if !elsewhere.exists() {
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        git(&elsewhere, &["init", "-q", "-b", "main"]);
        git(&elsewhere, &["config", "commit.gpgSign", "false"]);
    }
    git(
        &elsewhere,
        &[
            "fetch",
            "-q",
            tracker_store.as_str(),
            &format!("+refs/trackers/{tracker}/main:refs/remotes/store/main"),
        ],
    );
    git(
        &elsewhere,
        &["reset", "-q", "--hard", "refs/remotes/store/main"],
    );
    for file in files {
        std::fs::write(elsewhere.join(file), "x\n").expect("write");
        git(&elsewhere, &["add", "-A"]);
        git(
            &elsewhere,
            &["commit", "-q", "-m", &format!("{tracker}: {file}")],
        );
    }
    let tip = git(&elsewhere, &["rev-parse", "HEAD"]);
    git(
        &elsewhere,
        &[
            "push",
            "-q",
            tracker_store.as_str(),
            &format!("HEAD:refs/trackers/{tracker}/main"),
        ],
    );
    tip
}

/// The commit root's branch `branch` pins `tracker` at.
fn pin(store: &MetadataStore, branch: &str, tracker: &str) -> String {
    let tracker_store = store.paths().tracker_store.clone();
    git(
        &store.paths().project_root,
        &[
            &format!("--git-dir={tracker_store}"),
            "rev-parse",
            &format!("refs/trackers/root/{branch}:.newgit/pins/{tracker}"),
        ],
    )
}

#[test]
fn every_tracker_behind_moves_in_one_commit_that_lists_each() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let lib = advance(&temp, &store, "lib", &["a.js", "b.js"]);
    let ui = advance(&temp, &store, "ui", &["c.js"]);
    let repo = store.paths().project_root.clone();
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let manager = BranchManager::open(store.clone()).expect("manager");

    let pulled = manager.pull_all(None).expect("pull");
    let moved: Vec<(&str, usize)> = pulled
        .pulled
        .iter()
        .map(|pulled| (pulled.tracker.as_str(), pulled.commits))
        .collect();
    assert_eq!(moved, vec![("lib", 2), ("ui", 1)]);
    let view = pulled.view.expect("a view commit");
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        view,
        "the project root takes it"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Update trackers: lib (2 commits), ui (1 commit)"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD^@"]),
        before,
        "one commit on one parent"
    );
    assert_eq!(
        git(&repo, &["ls-tree", "-r", "--name-only", "HEAD", "libs/"]),
        "libs/lib/a.js\nlibs/lib/b.js\nlibs/lib/index.js\nlibs/ui/c.js\nlibs/ui/index.js"
    );
    assert_eq!(pin(&store, "instances/main", "lib"), lib);
    assert_eq!(pin(&store, "instances/main", "ui"), ui);

    // A second pull has nothing to do, and writes nothing.
    let again = manager.pull_all(None).expect("again");
    assert_eq!(again.view, None);
    assert!(again.pulled.is_empty(), "{again:?}");
    assert_eq!(
        again.up_to_date,
        vec!["access".to_owned(), "lib".to_owned(), "ui".to_owned()]
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), view);

    // It merges into `main` like any commit.
    manager.merge("main", "main").expect("merge");
    assert_eq!(pin(&store, "main", "lib"), lib);
    assert_eq!(pin(&store, "main", "ui"), ui);
}

#[test]
fn nothing_behind_makes_no_commit() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let manager = BranchManager::open(store.clone()).expect("manager");

    let pulled = manager.pull_all(None).expect("pull");
    assert_eq!(pulled.view, None);
    assert!(pulled.pulled.is_empty(), "{pulled:?}");
    // `access`, which holds the definitions, is a mounted tracker in Git too.
    assert_eq!(
        pulled.up_to_date,
        vec!["access".to_owned(), "lib".to_owned(), "ui".to_owned()]
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before, "no commit");
}

#[test]
fn in_an_instance_the_commit_lands_on_its_branch_and_main_keeps_its_pins() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(store.clone()).expect("manager");
    let workspace = manager
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    let lib_before = pin(&store, "main", "lib");
    let ui_before = pin(&store, "main", "ui");
    let lib = advance(&temp, &store, "lib", &["a.js"]);
    let ui = advance(&temp, &store, "ui", &["c.js", "d.js"]);
    let project_head = git(&repo, &["rev-parse", "HEAD"]);

    let pulled = manager.pull_all(Some("feature")).expect("pull");
    let view = pulled.view.expect("a view commit");
    assert_eq!(
        git(&workspace, &["rev-parse", "HEAD"]),
        view,
        "the workspace fast-forwards to it"
    );
    assert_eq!(
        git(&workspace, &["log", "-1", "--format=%s"]),
        "Update trackers: lib (1 commit), ui (2 commits)"
    );
    assert!(workspace.join("libs/lib/a.js").is_file());
    assert!(workspace.join("libs/ui/d.js").is_file());
    assert_eq!(pin(&store, "instances/feature", "lib"), lib);
    assert_eq!(pin(&store, "instances/feature", "ui"), ui);

    // `main`, and the project root, stay on their pins until it merges.
    assert_eq!(pin(&store, "main", "lib"), lib_before);
    assert_eq!(pin(&store, "main", "ui"), ui_before);
    assert_eq!(pin(&store, "instances/main", "lib"), lib_before);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), project_head);
    assert!(!repo.join("libs/lib/a.js").exists());
}

#[test]
fn a_pin_holding_its_own_commits_stays_and_the_rest_move() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = BranchManager::open(store.clone()).expect("manager");
    let workspace = manager
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    // The instance's own work on `lib`, while `lib`'s `main` moves too.
    std::fs::write(workspace.join("libs/lib/mine.js"), "mine\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "lib: mine"]);
    advance(&temp, &store, "lib", &["a.js"]);
    let ui = advance(&temp, &store, "ui", &["c.js"]);

    let pulled = manager.pull_all(Some("feature")).expect("pull");
    assert_eq!(
        pulled.diverged,
        vec![("lib".to_owned(), "main".to_owned())],
        "{pulled:?}"
    );
    let moved: Vec<&str> = pulled
        .pulled
        .iter()
        .map(|pulled| pulled.tracker.as_str())
        .collect();
    assert_eq!(moved, vec!["ui"]);
    assert_eq!(
        git(&workspace, &["log", "-1", "--format=%s"]),
        "Update trackers: ui (1 commit)"
    );
    assert!(
        workspace.join("libs/lib/mine.js").is_file(),
        "the instance's work on `lib` is still there"
    );
    assert!(!workspace.join("libs/lib/a.js").exists());
    assert_eq!(pin(&store, "instances/feature", "ui"), ui);
}

#[test]
fn trackers_outside_git_are_left_alone() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = BranchManager::open(store.clone()).expect("manager");
    let workspace = manager
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    let paths = store.paths();
    let lane = TrackerRepo::new(&paths.tracker_store, &paths.tracker_checkouts, "env");
    let lane_before = lane.main();
    assert!(lane_before.is_some(), "env has merged content");
    let env_before = std::fs::read_to_string(workspace.join(".env.local")).expect("read env");
    advance(&temp, &store, "lib", &["a.js"]);

    let pulled = manager.pull_all(Some("feature")).expect("pull");
    assert_eq!(pulled.outside_git, vec!["env".to_owned()]);
    assert!(
        pulled.pulled.iter().all(|pulled| pulled.tracker != "env"),
        "{pulled:?}"
    );
    assert_eq!(lane.main(), lane_before, "its lane does not move");
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("read env"),
        env_before,
        "nor its files in the instance"
    );
    assert_eq!(
        git(
            &workspace,
            &["ls-tree", "--name-only", "HEAD", ".env.local"]
        ),
        "",
        "and they never enter the commit"
    );
}
