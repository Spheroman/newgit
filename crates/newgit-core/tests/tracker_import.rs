//! `tracker create --code --from` (#140): another repository's history,
//! rewritten under a path, every commit and parent kept, joining the
//! project root's branch through one merge.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::compose::IMPORTED_NOTES_REF;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

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

fn who(name: &str, time: i64) -> Vec<(&'static str, String)> {
    vec![
        ("GIT_AUTHOR_NAME", name.to_owned()),
        ("GIT_AUTHOR_EMAIL", format!("{name}@example.com")),
        ("GIT_AUTHOR_DATE", format!("@{time} +0000")),
        ("GIT_COMMITTER_NAME", name.to_owned()),
        ("GIT_COMMITTER_EMAIL", format!("{name}@example.com")),
        ("GIT_COMMITTER_DATE", format!("@{time} +0000")),
    ]
}

fn commit(repo: &Utf8Path, files: &[(&str, &str)], message: &str, name: &str, time: i64) -> String {
    for (path, contents) in files {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
    }
    git(repo, &["add", "-A"]);
    git_env(
        repo,
        &["commit", "-q", "--no-gpg-sign", "-m", message],
        &who(name, time),
    );
    git(repo, &["rev-parse", "HEAD"])
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A project (two commits) and, separately, an outside repository older
/// than all of it, with a merge.
fn setup(temp: &Utf8Path) -> (MetadataStore, Utf8PathBuf) {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, &[("README.md", "hello\n")], "start", "ana", 5_000);
    commit(&repo, &[("README.md", "docs\n")], "docs", "ana", 6_000);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");

    let outside = temp.join("t3code");
    std::fs::create_dir_all(&outside).expect("mkdir");
    git(&outside, &["init", "-q", "-b", "newgit"]);
    commit(&outside, &[("app.js", "one\n")], "start", "bo", 1_000);
    git(&outside, &["checkout", "-q", "-b", "side"]);
    commit(&outside, &[("side.js", "side\n")], "side", "bo", 2_500);
    git(&outside, &["checkout", "-q", "newgit"]);
    commit(&outside, &[("app.js", "one\ntwo\n")], "two", "bo", 2_000);
    git_env(
        &outside,
        &[
            "merge",
            "-q",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            "merge side",
            "side",
        ],
        &who("bo", 3_000),
    );
    (store, outside)
}

fn path(path: &str) -> Vec<Utf8PathBuf> {
    vec![Utf8PathBuf::from(path)]
}

#[test]
fn an_import_joins_the_root_branch_through_one_merge() {
    let (_guard, temp) = tempdir();
    let (store, outside) = setup(&temp);
    let repo = store.paths().project_root.clone();
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let original = git(&outside, &["rev-parse", "newgit"]);

    let manager = BranchManager::open(store.clone()).expect("manager");
    let outcome = manager
        .import_code_tracker(
            "t3code",
            "project-devs",
            &path("apps/t3code"),
            outside.as_str(),
            Some("newgit"),
        )
        .expect("import");
    assert_eq!(outcome.commits, 4);
    assert_eq!(outcome.branch, "main");

    // One commit appended, nothing rewritten: the join, on top of the old tip.
    let head = git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(head, outcome.join);
    assert_eq!(git(&repo, &["rev-parse", "HEAD^1"]), before);
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD^2:apps/t3code"]),
        git(&outside, &["rev-parse", "newgit^{tree}"]),
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("apps/t3code/app.js")).expect("checked out"),
        "one\ntwo\n"
    );

    // The rewritten history: every commit and parent, authors and dates kept.
    let log = |dir: &Utf8Path, rev: &str| {
        git(
            dir,
            &["log", "--topo-order", "--format=%P|%an|%at|%ct|%s", rev],
        )
        .lines()
        .map(|line| {
            line.split_once('|')
                .map(|(parents, rest)| (parents.split(' ').count(), rest.to_owned()))
        })
        .collect::<Vec<_>>()
    };
    assert_eq!(log(&repo, "HEAD^2"), log(&outside, "newgit"));
    assert_eq!(
        git(
            &repo,
            &["log", "--format=%s", "HEAD", "--", "apps/t3code/app.js"]
        ),
        "two\nstart",
        "git log under the path reaches the imported history"
    );

    // The map from each original to its rewrite, which travels to the store.
    assert_eq!(
        git(
            &repo,
            &["notes", "--ref", IMPORTED_NOTES_REF, "show", &original]
        ),
        git(&repo, &["rev-parse", "HEAD^2"]),
    );
    let trackers = store.paths().tracker_store.clone();
    assert_eq!(
        git(&trackers, &["rev-parse", "refs/trackers/t3code/main"]),
        outcome.marker
    );
    assert_eq!(
        git(&trackers, &["rev-parse", "refs/imported/t3code"]),
        original
    );
    assert_eq!(
        git(&trackers, &["notes", "--ref", IMPORTED_NOTES_REF, "list"])
            .lines()
            .count(),
        4
    );

    // Importing again from the same commits gives the same ids.
    let again = temp.join("again");
    git(&temp, &["init", "-q", "-b", "main", again.as_str()]);
    git(&again, &["fetch", "-q", outside.as_str(), "newgit:refs/x"]);
    newgit_core::compose::import(&again, "refs/x", Utf8Path::new("apps/t3code"), "refs/y")
        .expect("import again");
    assert_eq!(
        git(&again, &["rev-parse", "refs/y"]),
        git(&repo, &["rev-parse", "HEAD^2"])
    );

    // A newgit that opens the project now composes the same branch, and
    // work on top of the join splits into the imported tracker.
    let manager = BranchManager::open(store.clone()).expect("reopen");
    manager.sync_project_root().expect("sync");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&repo, &["rev-parse", "refs/newgit/views/instances/main"]),
        head
    );
    commit(
        &repo,
        &[
            ("apps/t3code/app.js", "one\ntwo\nthree\n"),
            ("README.md", "both\n"),
        ],
        "work",
        "ana",
        7_000,
    );
    let work = git(&repo, &["rev-parse", "HEAD"]);
    manager.sync_project_root().expect("split the work");
    let t3code = git(
        &repo,
        &["rev-parse", "refs/newgit/trackers/t3code/instances/main"],
    );
    assert_eq!(
        git(&repo, &["rev-parse", &format!("{t3code}^")]),
        outcome.marker
    );
    assert_eq!(
        git(&repo, &["show", &format!("{t3code}:apps/t3code/app.js")]),
        "one\ntwo\nthree"
    );
    let manager = BranchManager::open(store).expect("reopen");
    manager.sync_project_root().expect("sync again");
    assert_eq!(
        git(&repo, &["rev-parse", "refs/newgit/views/instances/main"]),
        work,
        "the work composes back as itself"
    );
}

#[test]
fn an_import_refuses_a_path_that_has_files() {
    let (_guard, temp) = tempdir();
    let (store, outside) = setup(&temp);
    let repo = store.paths().project_root.clone();
    commit(
        &repo,
        &[("apps/t3code/old.js", "old\n")],
        "old",
        "ana",
        6_500,
    );
    let manager = BranchManager::open(store).expect("manager");
    let error = manager
        .import_code_tracker(
            "t3code",
            "project-devs",
            &path("apps/t3code"),
            outside.as_str(),
            None,
        )
        .expect_err("refused");
    assert!(error.to_string().contains("already has files"), "{error}");
}

#[test]
fn an_import_refuses_history_that_adds_a_secret() {
    let (_guard, temp) = tempdir();
    let (store, outside) = setup(&temp);
    let repo = store.paths().project_root.clone();
    commit(&outside, &[(".env", "TOKEN=1\n")], "env", "bo", 3_500);
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager
        .create_tracker("secrets", "user", Storage::Local, false)
        .expect("secret tracker");
    let manager = BranchManager::open(store.clone()).expect("reopen");
    manager
        .track_paths("secrets", &path("apps/t3code/.env"))
        .expect("track");
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let manager = BranchManager::open(store.clone()).expect("reopen");
    let error = manager
        .import_code_tracker(
            "t3code",
            "project-devs",
            &path("apps/t3code"),
            outside.as_str(),
            None,
        )
        .expect_err("refused");
    assert!(
        error.to_string().contains("secret tracker `secrets`"),
        "{error}"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert!(!repo.join("apps/t3code").exists());
    assert!(!store.paths().trackers.join("t3code.toml").exists());
    assert_eq!(
        git(
            &repo,
            &[
                "for-each-ref",
                "refs/newgit/trackers/t3code",
                "refs/newgit/imported"
            ]
        ),
        ""
    );
}
