//! The checks the Worker asks the Git endpoint's container for, since it
//! cannot run Git itself: how a merge's pins move along each tracker's
//! history, and whether an index is what its root commit pins.

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::endpoint::{
    AncestryCheck, AncestryPlan, IndexPlan, Source, check_ancestry, check_index,
};
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

fn repos(trackers: &BTreeMap<String, (Utf8PathBuf, String)>) -> BTreeMap<String, Source> {
    trackers
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
        .collect()
}

#[test]
fn the_index_check_holds_an_index_to_what_its_root_commit_pins() {
    let (_guard, temp) = tempdir();
    let (_, trackers) = published(&temp);
    let list: BTreeMap<String, String> = trackers
        .iter()
        .map(|(tracker, (_, commit))| (tracker.clone(), commit.clone()))
        .collect();
    let right = check_index(&IndexPlan {
        dir: temp.join("container"),
        repos: repos(&trackers),
        list: list.clone(),
    })
    .expect("check");
    assert!(right.matches, "{:?} vs {list:?}", right.actual);

    let mut wrong = list.clone();
    wrong.insert("payments".to_owned(), list["access"].clone());
    let answer = check_index(&IndexPlan {
        dir: temp.join("container"),
        repos: repos(&trackers),
        list: wrong,
    })
    .expect("check");
    assert!(!answer.matches);
    assert_eq!(answer.actual, list, "it says what root really pins");
}

#[test]
fn the_ancestry_check_tells_forward_on_main_from_back_and_off_main() {
    let (_guard, temp) = tempdir();
    let (_, trackers) = published(&temp);
    let (payments, tip) = trackers["payments"].clone();
    // One more commit on payments' `main`, and one on a side branch.
    let work = temp.join("payments-work");
    git(&temp, &["clone", "-q", payments.as_str(), work.as_str()]);
    std::fs::write(work.join("more.txt"), "more\n").expect("write");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "more"]);
    let ahead = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["push", "-q", "origin", "HEAD:main"]);
    git(&work, &["checkout", "-q", "-b", "side", &tip]);
    std::fs::write(work.join("side.txt"), "side\n").expect("write");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "side"]);
    let side = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["push", "-q", "origin", "HEAD:side"]);

    let ask = |old: &str, new: &str| {
        check_ancestry(&AncestryPlan {
            dir: temp.join("container"),
            repos: repos(&trackers),
            checks: vec![AncestryCheck {
                tracker: "payments".to_owned(),
                old: old.to_owned(),
                new: new.to_owned(),
            }],
        })
        .expect("check")
        .remove(0)
    };
    let forward = ask(&tip, &ahead);
    assert!(forward.forward && forward.on_main, "{forward:?}");
    let back = ask(&ahead, &tip);
    assert!(
        !back.forward && back.on_main,
        "a rollback along main: {back:?}"
    );
    let off = ask(&tip, &side);
    assert!(off.forward && !off.on_main, "{off:?}");
    let foreign = ask(&tip, &trackers["root"].1);
    assert!(
        !foreign.forward && !foreign.on_main,
        "another tracker's commit: {foreign:?}"
    );
}
