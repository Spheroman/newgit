//! A view rooted at a tracker, as `/<owner>/<name>/<path>.git` asks for
//! (docs/design/anchoring.md, *Step 8*): the tracker's own files at the top,
//! its children mounted, composed from its commit, not root's. Root's view is
//! unchanged.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::endpoint::{Plan, Source, compose_view};
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

/// A project with a tracker in Git carved out, its trackers
/// published as bare repos the way the remote holds them. Returns the
/// project's HEAD and each tracker's (repo, main).
fn published(temp: &Utf8Path) -> (String, BTreeMap<String, (Utf8PathBuf, String)>) {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("apps/payments")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("apps/payments/README.md"), "payments\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    BranchManager::open(store.clone())
        .expect("manager")
        .create_git_tracker(
            "payments",
            "user:jack",
            &[Utf8PathBuf::from("apps/payments")],
        )
        .expect("carve");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);
    // Made after the carve: only payments' history has it. (What was
    // committed before stays in root's history, readable by root's readers.)
    std::fs::write(repo.join("apps/payments/key.txt"), "PAYMENTS-ONLY\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "the key"]);
    let manager = BranchManager::open(store.clone()).expect("reopen");
    manager.sync_project_root().expect("split");

    let trackers_git = store.paths().tracker_store.clone();
    let mut out = BTreeMap::new();
    for tracker in ["root", "access", "payments"] {
        let bare = temp.join(format!("remote/{tracker}.git"));
        git(temp, &["init", "-q", "--bare", "-b", "main", bare.as_str()]);
        let tip = ["instances/main", "main"]
            .iter()
            .find_map(|branch| {
                Command::new("git")
                    .args([
                        &format!("--git-dir={trackers_git}"),
                        "rev-parse",
                        "--verify",
                        "-q",
                        &format!("refs/trackers/{tracker}/{branch}"),
                    ])
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            })
            .expect("tracker has history");
        git(
            temp,
            &[
                &format!("--git-dir={trackers_git}"),
                "push",
                "-q",
                bare.as_str(),
                &format!("{tip}:refs/heads/main"),
            ],
        );
        out.insert(tracker.to_owned(), (bare, tip));
    }
    (git(&repo, &["rev-parse", "HEAD"]), out)
}

fn plan(
    dir: &Utf8Path,
    trackers: &BTreeMap<String, (Utf8PathBuf, String)>,
    readable: &[&str],
) -> Plan {
    let list = readable
        .iter()
        .map(|tracker| ((*tracker).to_owned(), trackers[*tracker].1.clone()))
        .collect();
    Plan {
        root: readable[0].to_owned(),
        dir: dir.to_path_buf(),
        branches: BTreeMap::from([("main".to_owned(), list)]),
        default_branch: "main".to_owned(),
        repos: trackers
            .iter()
            .map(|(tracker, (repo, _))| {
                (
                    tracker.clone(),
                    Source {
                        url: repo.to_string(),
                        auth: "Basic eDpwdw==".to_owned(),
                    },
                )
            })
            .collect(),
    }
}

#[test]
fn a_view_rooted_at_a_tracker_has_its_files_at_the_top() {
    let (_guard, temp) = tempdir();
    let (_, trackers) = published(&temp);
    let container = temp.join("container");

    let rooted = compose_view(&plan(&container, &trackers, &["payments"])).expect("payments' view");
    assert_eq!(git(&rooted, &["show", "main:README.md"]), "payments");
    assert_eq!(git(&rooted, &["show", "main:key.txt"]), "PAYMENTS-ONLY");
    assert!(
        !git(&rooted, &["ls-tree", "--name-only", "main"]).contains("apps"),
        "nothing of root's"
    );

    let whole = compose_view(&plan(
        &container,
        &trackers,
        &["root", "access", "payments"],
    ))
    .expect("root's view");
    assert_eq!(git(&whole, &["show", "main:README.md"]), "hello");
    assert_eq!(
        git(&whole, &["show", "main:apps/payments/README.md"]),
        "payments"
    );
    assert_ne!(rooted, whole, "two views");
}
