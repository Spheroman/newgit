//! Hazards in the projection found in review (docs/design/trackers.md, step 3c):
//! a note written before a carve, a secret hidden behind a merge, deleting a
//! secret, the project root's own branch, an instance named `main`, and a
//! removed instance's commits collected away.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::compose;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::{Storage, TrackerDefinition};

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

fn commit_as(
    repo: &Utf8Path,
    author: (&str, &str),
    files: &[(&str, &str)],
    message: &str,
    time: i64,
) {
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
            who("GIT_AUTHOR_NAME", author.0.to_owned()),
            who("GIT_AUTHOR_EMAIL", author.1.to_owned()),
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

fn commit(repo: &Utf8Path, files: &[(&str, &str)], message: &str, time: i64) {
    commit_as(repo, ("ana", "ana@example.com"), files, message, time);
}

/// A project with app code and an Electron app, initialized, with one more
/// commit made in the project root after `newgit init`.
fn setup(temp: &Utf8Path) -> (MetadataStore, Utf8PathBuf) {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(
        &repo,
        &[("README.md", "hello\n"), ("apps/electron/main.js", "v1\n")],
        "initial",
        1_000,
    );
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    (store, repo)
}

fn trackers(store: &MetadataStore) -> Utf8PathBuf {
    store.paths().tracker_store.clone()
}

/// The view commit a tracker's tip was split from.
fn split_source(store: &MetadataStore, reference: &str) -> String {
    git(
        &trackers(store),
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Newgit-Split,valueonly)",
            reference,
        ],
    )
}

/// An instance's view, composed from root's `instances/<slug>` alone — what
/// its branch must be, commit for commit.
fn recompose(_manager: &BranchManager, repo: &Utf8Path, slug: &str) -> Option<String> {
    let root = git(
        repo,
        &[
            "rev-parse",
            &format!("refs/newgit/trackers/root/instances/{slug}"),
        ],
    );
    let everything = |_: &str| true;
    compose::compose(
        repo,
        "root",
        &[("refs/newgit/views/check".to_owned(), root)],
        &compose::ComposeOptions {
            readable: &everything,
            admit: None,
        },
    )
    .expect("compose")
    .remove(0)
    .tip
}

fn workspace_commit(
    manager: &BranchManager,
    instance: &str,
    files: &[(&str, &str)],
    message: &str,
) {
    let workspace = manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .workspace_path;
    commit(&workspace, files, message, 5_000);
    manager.checkpoint(instance, None).expect("checkpoint");
}

fn tree(repo: &Utf8Path, reference: &str) -> String {
    git(repo, &["ls-tree", "-r", "--name-only", reference])
}

/// Carving is a commit on the project root's branch: an instance forked
/// before it keeps the carved files in root, as its history has them, until
/// it merges that commit — and then the tracker holds every file, at its own
/// root, the instance's work included.
#[test]
fn work_from_before_a_carve_moves_to_the_tracker_when_the_carve_is_merged() {
    let (_guard, temp) = tempdir();
    let (store, repo) = setup(&temp);
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");
    workspace_commit(
        &manager,
        "feature-a",
        &[("apps/electron/new.js", "new\n")],
        "electron work, before electron is a tracker",
    );
    manager
        .tracker_log_target("root", Some("feature-a"))
        .expect("split as root");

    manager
        .create_git_tracker(
            "electron",
            "project-devs",
            &[Utf8PathBuf::from("apps/electron")],
        )
        .expect("carve");
    let manager = BranchManager::open(store.clone()).expect("reopen");
    let trackers = trackers(&store);
    assert!(
        tree(&trackers, "refs/trackers/root/instances/feature-a").contains("apps/electron/new.js"),
        "before it takes the carve, the instance's work is root's"
    );
    let workspace = manager
        .store()
        .find_branch("feature-a")
        .expect("instance")
        .workspace_path;
    git(&workspace, &["fetch", "-q", repo.as_str(), "main"]);
    git_env(
        &workspace,
        &["merge", "-q", "--no-edit", "FETCH_HEAD"],
        &[("GIT_COMMITTER_DATE", "@6000 +0000".to_owned())],
    );
    workspace_commit(
        &manager,
        "feature-a",
        &[
            ("README.md", "more\n"),
            ("apps/electron/other.js", "other\n"),
        ],
        "more electron work, after the carve",
    );
    manager
        .tracker_log_target("electron", Some("feature-a"))
        .expect("split again");

    assert_eq!(
        tree(&trackers, "refs/trackers/electron/instances/feature-a"),
        "main.js\nnew.js\nother.js",
        "electron holds every file at its own root, the work from before the carve too"
    );
    assert!(
        !tree(&trackers, "refs/trackers/root/instances/feature-a").contains("apps/electron"),
        "and root no longer does"
    );
    let head = git(
        &store.paths().project_root,
        &["rev-parse", "refs/heads/feature-a"],
    );
    assert_eq!(
        recompose(&manager, &store.paths().project_root, "feature-a").as_deref(),
        Some(head.as_str()),
        "root's branch composes back into the instance's exact commits"
    );
}

#[test]
fn a_secret_behind_a_merge_is_found() {
    let (_guard, temp) = tempdir();
    let (store, repo) = setup(&temp);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join(".env.local"), "TOKEN=leak\n").expect("write");
    git(&repo, &["add", "-f", ".env.local"]);
    git(&repo, &["commit", "-q", "--no-gpg-sign", "-m", "add"]);
    git(&repo, &["rm", "-q", "--cached", ".env.local"]);
    git(&repo, &["commit", "-q", "--no-gpg-sign", "-m", "remove"]);
    git(&repo, &["checkout", "-q", "main"]);
    commit(&repo, &[("README.md", "main\n")], "main moves", 6_000);
    git(
        &repo,
        &["merge", "-q", "--no-edit", "--no-gpg-sign", "side"],
    );

    let manager = BranchManager::open(store).expect("manager");
    manager
        .create_tracker("env", "user", Storage::Local, false)
        .expect("tracker");
    let mut all: Vec<TrackerDefinition> = vec![manager.root_tracker().clone()];
    let mut env = TrackerDefinition::new(
        "env",
        "user".to_owned(),
        Storage::Local,
        false,
        vec![Utf8PathBuf::from(".env.local")],
    )
    .expect("definition");
    env.git = false;
    all.push(env);
    let found =
        compose::find_outside_git(&repo, &["HEAD", "--not", &base], &[], &all).expect("scan");
    assert_eq!(
        found.map(|(_, path, tracker)| (path, tracker)),
        Some((".env.local".to_owned(), "env".to_owned())),
        "added on a side branch and deleted before the merge still rode along"
    );
    // Deleting a secret is how it comes out: never refused.
    let removal = git(&repo, &["rev-parse", "side"]);
    let found = compose::find_outside_git(
        &repo,
        &[&removal, "--not", &format!("{removal}~1")],
        &[],
        &all,
    )
    .expect("scan");
    assert_eq!(found, None);
}

#[test]
fn the_project_roots_branches_follow_its_own_branch_not_a_checkout() {
    let (_guard, temp) = tempdir();
    let (store, repo) = setup(&temp);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "-b", "experiment"]);
    commit(&repo, &[("README.md", "experiment\n")], "experiment", 2_000);
    BranchManager::open(store.clone())
        .expect("manager")
        .spawn("feature-a", None)
        .expect("spawn");
    let trackers = store.paths().tracker_store.clone();
    let root_tip = git(
        &trackers,
        &["rev-parse", "refs/trackers/root/instances/main"],
    );
    // `main` is history newgit adopted, its own root commit, or one split.
    let expected = compose::read_root(&repo, &main)
        .expect("note")
        .map_or(main, |(_, commit)| commit);
    assert_eq!(root_tip, expected, "still main's state");
}

#[test]
fn an_instance_may_not_be_named_main() {
    let (_guard, temp) = tempdir();
    let (store, _repo) = setup(&temp);
    let error = BranchManager::open(store)
        .expect("manager")
        .spawn("main", None)
        .expect_err("main is the project root");
    assert!(error.to_string().contains("project root"), "{error}");
}

#[test]
fn a_removed_instances_commits_collected_away_do_not_break_a_split() {
    let (_guard, temp) = tempdir();
    let (store, repo) = setup(&temp);
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");
    workspace_commit(
        &manager,
        "feature-a",
        &[("feature.txt", "work\n")],
        "feature",
    );
    manager
        .tracker_log_target("root", Some("feature-a"))
        .expect("split");
    // The project root takes the work, the instance goes, and Git collects
    // everything only the instance's refs held.
    git(&repo, &["merge", "-q", "--ff-only", "feature-a"]);
    manager
        .remove(
            "feature-a",
            &repo,
            newgit_core::cleanup::ArchivedCheckpoints::Keep,
            newgit_core::manager::SourceBranchPolicy::Auto,
        )
        .expect("remove");
    for dir in [repo.clone(), store.paths().tracker_store.clone()] {
        git(&dir, &["reflog", "expire", "--expire=now", "--all"]);
        git(&dir, &["gc", "-q", "--prune=now"]);
    }

    let outcome = BranchManager::open(store.clone())
        .expect("reopen")
        .spawn("feature-b", None)
        .expect("spawn");
    assert_eq!(outcome.projection_warning, None);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        split_source(&store, "refs/trackers/root/instances/main"),
        head
    );
}
