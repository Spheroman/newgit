//! Code trackers (docs/design/trackers.md, step 3b): committed content with its
//! own history, carved out of whichever tracker owned its paths, so composing
//! still reproduces the project's history exactly.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::compose::{self, TrackerTip};
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
fn init_writes_the_project_tracker_which_carries_newgit() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    assert!(store.paths().trackers.join("project.toml").is_file());
    let manager = BranchManager::open(store).expect("manager");
    let project = manager
        .code_trackers()
        .iter()
        .find(|definition| definition.name == "project")
        .expect("project");
    assert_eq!(project.paths, vec![Utf8PathBuf::from(".newgit")]);
    assert!(
        manager
            .root_tracker()
            .excluded
            .contains(&Utf8PathBuf::from(".newgit")),
        "root does not own .newgit"
    );
}

#[test]
fn a_carved_code_tracker_composes_back_into_the_project_history() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(store.clone()).expect("manager");
    let outcome = manager
        .create_code_tracker(
            "electron",
            "project-devs",
            &[Utf8PathBuf::from("apps/electron")],
        )
        .expect("carve");
    assert_eq!(outcome.source, "root");
    assert_eq!(
        outcome.commits, 2,
        "the two commits that touched apps/electron"
    );

    // The tracker's own history: only its files, each commit marked with its source.
    let trackers = store.paths().tracker_store.clone();
    let main = "refs/trackers/electron/main";
    assert_eq!(
        git(&trackers, &["ls-tree", "-r", "--name-only", main]),
        "apps/electron/main.js"
    );
    let trailers = git(
        &trackers,
        &[
            "log",
            "--format=%(trailers:key=Newgit-Split,valueonly)",
            main,
        ],
    );
    assert_eq!(trailers.lines().filter(|line| !line.is_empty()).count(), 2);

    // Composing root and electron reproduces the project's exact commits.
    let manager = BranchManager::open(store.clone()).expect("reopen");
    let electron = manager
        .code_trackers()
        .iter()
        .find(|definition| definition.name == "electron")
        .expect("electron");
    assert!(
        manager
            .root_tracker()
            .excluded
            .contains(&Utf8PathBuf::from("apps/electron"))
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let electron_tip = git(&repo, &["rev-parse", "refs/newgit/trackers/electron/main"]);
    let view = compose::compose(
        &repo,
        "refs/newgit/views/check",
        &[
            TrackerTip {
                definition: manager.root_tracker(),
                commit: head.clone(),
            },
            TrackerTip {
                definition: electron,
                commit: electron_tip,
            },
        ],
        &[],
    )
    .expect("compose");
    assert_eq!(view.tip.as_deref(), Some(head.as_str()));
}

#[test]
fn a_code_tracker_is_committed_to_not_captured() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = BranchManager::open(store.clone()).expect("manager");
    manager
        .create_code_tracker(
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
    assert!(capture.to_string().contains("code tracker"), "{capture}");
    let track = manager
        .track_paths("electron", &[Utf8PathBuf::from("apps/desktop")])
        .expect_err("paths are set at creation");
    assert!(track.to_string().contains("code tracker"), "{track}");
}
