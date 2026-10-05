//! The Git endpoint's composer: a reader's view holds exactly the trackers
//! the plan lists — the author's commits when that is everything, and not one
//! object of a tracker left out.

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

/// A project with a secret-feeling code tracker carved out, its trackers
/// published as bare repos the way the remote holds them. Returns the
/// project's HEAD and each tracker's (repo, main).
fn published(temp: &Utf8Path) -> (String, BTreeMap<String, (Utf8PathBuf, String)>) {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("apps/payments")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("apps/payments/key.txt"), "PAYMENTS-ONLY\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    BranchManager::open(store.clone())
        .expect("manager")
        .create_code_tracker(
            "payments",
            "user:jack",
            &[Utf8PathBuf::from("apps/payments")],
        )
        .expect("carve");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);
    let manager = BranchManager::open(store.clone()).expect("reopen");
    manager.sync_project_root().expect("split");

    let trackers_git = store.paths().tracker_store.clone();
    let mut out = BTreeMap::new();
    for tracker in ["root", "project", "access", "payments"] {
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
        dir: dir.to_path_buf(),
        branches: BTreeMap::from([("main".to_owned(), list)]),
        default_branch: "main".to_owned(),
        definitions: BTreeMap::from([("access".to_owned(), trackers["access"].1.clone())]),
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
fn a_reader_of_everything_gets_the_projects_own_commits() {
    let (_guard, temp) = tempdir();
    let (head, trackers) = published(&temp);
    let view = compose_view(&plan(
        &temp.join("container"),
        &trackers,
        &["root", "project", "access", "payments"],
    ))
    .expect("compose");
    assert_eq!(git(&view, &["rev-parse", "main"]), head);
    // Cached: the same plan is the same repository.
    let again = compose_view(&plan(
        &temp.join("container"),
        &trackers,
        &["root", "project", "access", "payments"],
    ))
    .expect("compose again");
    assert_eq!(again, view);
}

#[test]
fn a_tracker_left_out_of_the_plan_leaves_not_one_object_behind() {
    let (_guard, temp) = tempdir();
    let (_, trackers) = published(&temp);
    let container = temp.join("container");
    // Someone who may read everything fetches first, so the container holds it all.
    compose_view(&plan(
        &container,
        &trackers,
        &["root", "project", "access", "payments"],
    ))
    .expect("full view");
    let view = compose_view(&plan(&container, &trackers, &["root", "project", "access"]))
        .expect("restricted view");

    let files = git(&view, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!files.contains("apps/payments"), "{files}");

    // Every rebuilt commit names the commit it stands for, and only commits
    // this view holds have a note.
    let notes = git(&view, &["notes", "list"]);
    assert!(!notes.is_empty(), "the restricted view rebuilt commits");
    for line in notes.lines() {
        let (_, commit) = line.split_once(' ').expect("note line");
        let note = git(&view, &["notes", "show", commit]);
        assert!(note.starts_with("canonical "), "{note}");
    }
    let objects = git(
        &view,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    for object in objects.lines() {
        assert!(
            !git(&view, &["cat-file", "-p", object]).contains("PAYMENTS-ONLY"),
            "no object of the payments tracker is in the restricted view"
        );
    }
}

#[test]
fn a_branch_naming_another_trackers_commit_is_not_served() {
    let (_guard, temp) = tempdir();
    let (head, trackers) = published(&temp);
    let container = temp.join("container");
    // Someone who may read payments fetched first: its commit is in the
    // container's store.
    compose_view(&plan(
        &container,
        &trackers,
        &["root", "project", "access", "payments"],
    ))
    .expect("full view");

    // A branch that names payments' commit as `root`'s.
    let mut forged = plan(&container, &trackers, &["root", "project", "access"]);
    let mut leak = forged.branches["main"].clone();
    leak.insert("root".to_owned(), trackers["payments"].1.clone());
    forged.branches.insert("leak".to_owned(), leak);
    let view = compose_view(&forged).expect("main is still served");

    let branches = git(
        &view,
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    );
    assert_eq!(branches, "refs/heads/main", "the forged branch is left out");
    let objects = git(
        &view,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    for object in objects.lines() {
        assert!(!git(&view, &["cat-file", "-p", object]).contains("PAYMENTS-ONLY"));
    }
    let _ = head;
}
