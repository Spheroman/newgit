//! Step 3c (docs/design/trackers.md): the project root is the instance `main`,
//! its commits split into its tracker branches before anything is composed;
//! code trackers get their history the first time it is needed; a history
//! that once committed a now-secret file composes unchanged; and split
//! commits are signed when you sign your commits.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::compose::{self, TrackerTip};
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

/// The project root's view, composed from every code tracker's
/// `instances/main` — what `HEAD` must be, commit for commit.
fn recompose(manager: &BranchManager, repo: &Utf8Path) -> Option<String> {
    let definitions: Vec<&TrackerDefinition> = std::iter::once(manager.root_tracker())
        .chain(manager.code_trackers())
        .collect();
    let tips: Vec<TrackerTip<'_>> = definitions
        .into_iter()
        .filter_map(|definition| {
            let output = Command::new("git")
                .args(["-C", repo.as_str(), "rev-parse", "--verify", "-q"])
                .arg(format!(
                    "refs/newgit/trackers/{}/instances/main",
                    definition.name
                ))
                .output()
                .expect("git runs");
            output.status.success().then(|| TrackerTip {
                definition,
                commit: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            })
        })
        .collect();
    compose::compose(
        repo,
        "refs/newgit/views/check",
        &tips,
        manager.code_trackers(),
    )
    .expect("compose")
    .tip
}

#[test]
fn project_root_commits_are_split_into_its_branches_and_recompose_exactly() {
    let (_guard, temp) = tempdir();
    let (store, repo) = setup(&temp);
    commit(
        &repo,
        &[("README.md", "docs\n"), ("apps/electron/main.js", "v2\n")],
        "after init",
        2_000,
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let manager = BranchManager::open(store.clone()).expect("manager");
    let outcome = manager.spawn("feature-a", None).expect("spawn");
    assert_eq!(outcome.projection_warning, None);
    assert_eq!(
        split_source(&store, "refs/trackers/root/instances/main"),
        head
    );

    // Composed back from the trackers, it is the project root's history.
    assert_eq!(recompose(&manager, &repo).as_deref(), Some(head.as_str()));
}

#[test]
fn a_reset_in_the_project_root_moves_its_branches_back() {
    let (_guard, temp) = tempdir();
    let (store, repo) = setup(&temp);
    commit(&repo, &[("README.md", "one\n")], "one", 2_000);
    let one = git(&repo, &["rev-parse", "HEAD"]);
    commit(&repo, &[("README.md", "two\n")], "two", 3_000);
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");
    git(&repo, &["reset", "-q", "--hard", &one]);
    manager.spawn("feature-b", None).expect("spawn");
    assert_eq!(
        split_source(&store, "refs/trackers/root/instances/main"),
        one,
        "the project root's Git is the truth for its own branches"
    );
}

#[test]
fn a_code_tracker_defined_by_hand_is_carved_the_first_time_it_is_needed() {
    let (_guard, temp) = tempdir();
    let (store, _repo) = setup(&temp);
    let mut electron = TrackerDefinition::new(
        "electron",
        "project-devs".to_owned(),
        Storage::Local,
        true,
        vec![Utf8PathBuf::from("apps/electron")],
    )
    .expect("definition");
    electron.secret = false;
    store
        .create_tracker_definition(&electron)
        .expect("write definition");
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");
    assert_eq!(
        git(
            &trackers(&store),
            &[
                "ls-tree",
                "-r",
                "--name-only",
                "refs/trackers/electron/main"
            ],
        ),
        "apps/electron/main.js"
    );
}

#[test]
fn carving_after_the_project_root_split_still_recomposes_its_commits() {
    let (_guard, temp) = tempdir();
    let (store, repo) = setup(&temp);
    commit(
        &repo,
        &[("README.md", "both\n"), ("apps/electron/main.js", "v2\n")],
        "both at once",
        2_000,
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    BranchManager::open(store.clone())
        .expect("manager")
        .spawn("feature-a", None)
        .expect("spawn");
    BranchManager::open(store.clone())
        .expect("manager")
        .create_code_tracker(
            "electron",
            "project-devs",
            &[Utf8PathBuf::from("apps/electron")],
        )
        .expect("carve");

    let manager = BranchManager::open(store.clone()).expect("reopen");
    let outcome = manager.spawn("feature-b", None).expect("spawn");
    assert_eq!(outcome.projection_warning, None);
    assert_eq!(
        split_source(&store, "refs/trackers/electron/instances/main"),
        head,
        "the project root's unmerged commit reached the carved tracker"
    );
    assert_eq!(recompose(&manager, &repo).as_deref(), Some(head.as_str()));
}

#[test]
fn history_that_once_committed_a_now_secret_file_composes_unchanged() {
    let (_guard, temp) = tempdir();
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, &[("README.md", "hello\n")], "initial", 1_000);
    commit(
        &repo,
        &[(".env.local", "TOKEN=old\n")],
        "committed by mistake",
        2_000,
    );
    git(&repo, &["rm", "-q", "--cached", ".env.local"]);
    commit(
        &repo,
        &[(".gitignore", ".env.local\n")],
        "stop tracking it",
        3_000,
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");

    let manager = BranchManager::open(store.clone()).expect("manager");
    manager
        .create_tracker("env", "user", Storage::Local, false)
        .expect("tracker");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    let manager = BranchManager::open(store.clone()).expect("reopen");
    let outcome = manager.spawn("feature-a", None).expect("spawn");
    assert_eq!(outcome.projection_warning, None);
    assert_eq!(
        recompose(&manager, &repo).as_deref(),
        Some(head.as_str()),
        "the history composes where it is, ids unchanged"
    );
}

#[test]
fn split_commits_are_signed_as_their_author_only_when_you_sign() {
    let (_guard, temp) = tempdir();
    let key = temp.join("signing-key");
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", key.as_str()])
        .status();
    if !made.is_ok_and(|status| status.success()) {
        eprintln!("ssh-keygen unavailable; skipping");
        return;
    }
    let (store, repo) = setup(&temp);
    for (key_name, value) in [
        ("gpg.format", "ssh"),
        ("user.signingkey", key.as_str()),
        ("commit.gpgSign", "true"),
        ("user.email", "ana@example.com"),
    ] {
        git(&repo, &["config", key_name, value]);
    }
    commit(&repo, &[("README.md", "mine\n")], "mine", 2_000);
    commit_as(
        &repo,
        ("bo", "bo@example.com"),
        &[("README.md", "theirs\n")],
        "theirs",
        3_000,
    );
    BranchManager::open(store.clone())
        .expect("manager")
        .spawn("feature-a", None)
        .expect("spawn");

    let raw = |reference: &str| git(&trackers(&store), &["cat-file", "commit", reference]);
    let theirs = raw("refs/trackers/root/instances/main");
    let mine = raw("refs/trackers/root/instances/main~1");
    assert!(
        mine.contains("gpgsig"),
        "your commit is signed with your key"
    );
    assert!(
        !theirs.contains("gpgsig"),
        "newgit never signs on someone else's behalf"
    );
}
