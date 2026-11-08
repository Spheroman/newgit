//! The Git endpoint serves tags: each a frozen list, composed for its reader
//! as a branch is, at `refs/tags/<name>`. An annotated one is made again at
//! the view's commit, the same object every time.

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::endpoint::{Plan, PlanTag, Source, TagAnnotation, compose_view};
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
        root: "root".to_owned(),
        dir: dir.to_path_buf(),
        branches: BTreeMap::from([("main".to_owned(), list)]),
        default_branch: "main".to_owned(),
        tags: BTreeMap::new(),
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

fn tagged(mut plan: Plan, name: &str, annotation: Option<TagAnnotation>) -> Plan {
    let list = plan.branches["main"].clone();
    plan.tags
        .insert(name.to_owned(), PlanTag { list, annotation });
    plan
}

#[test]
fn a_tag_is_served_at_the_view_commit_its_reader_sees() {
    let (_guard, temp) = tempdir();
    let (head, trackers) = published(&temp);
    let container = temp.join("container");

    let all = tagged(
        plan(&container, &trackers, &["root", "access", "payments"]),
        "v1.0.0",
        None,
    );
    let view = compose_view(&all).expect("compose");
    assert_eq!(git(&view, &["rev-parse", "refs/tags/v1.0.0"]), head);
    assert_eq!(git(&view, &["cat-file", "-t", "v1.0.0"]), "commit");

    // Someone who may not read payments gets the tag at their own view's
    // commit: the same commit their `main` is.
    let some = tagged(
        plan(&container, &trackers, &["root", "access"]),
        "v1.0.0",
        None,
    );
    let restricted = compose_view(&some).expect("restricted");
    let tip = git(&restricted, &["rev-parse", "main"]);
    assert_ne!(tip, head);
    assert_eq!(git(&restricted, &["rev-parse", "refs/tags/v1.0.0"]), tip);
}

#[test]
fn an_annotated_tag_keeps_its_message_and_is_the_same_object_every_time() {
    let (_guard, temp) = tempdir();
    let (head, trackers) = published(&temp);
    let annotation = TagAnnotation {
        tagger: "Ana <ana@example.com> 1700000000 +0100".to_owned(),
        message: "Release 1.0\n\nThe first one.\n".to_owned(),
    };
    let make = |dir: &str| {
        let view = compose_view(&tagged(
            plan(&temp.join(dir), &trackers, &["root", "access", "payments"]),
            "release/1.0",
            Some(annotation.clone()),
        ))
        .expect("compose");
        git(&view, &["rev-parse", "refs/tags/release/1.0"])
    };
    let first = make("one");
    let second = make("two");
    assert_eq!(
        first, second,
        "the same annotation at the same commit is one object"
    );

    let view = temp.join("one/views");
    let view = std::fs::read_dir(&view)
        .expect("views")
        .map(|entry| Utf8PathBuf::from_path_buf(entry.expect("entry").path()).expect("utf8"))
        .find(|path| path.extension() == Some("git"))
        .expect("a view");
    assert_eq!(git(&view, &["cat-file", "-t", &first]), "tag");
    assert_eq!(git(&view, &["rev-parse", "release/1.0^{commit}"]), head);
    let object = git(&view, &["cat-file", "-p", &first]);
    assert!(
        object.contains("tagger Ana <ana@example.com> 1700000000 +0100"),
        "{object}"
    );
    assert!(
        object.ends_with("Release 1.0\n\nThe first one."),
        "{object}"
    );
}

#[test]
fn a_tag_naming_a_commit_root_does_not_have_is_left_out() {
    let (_guard, temp) = tempdir();
    let (head, trackers) = published(&temp);
    let mut plan = plan(
        &temp.join("container"),
        &trackers,
        &["root", "access", "payments"],
    );
    plan.tags.insert(
        "bogus".to_owned(),
        PlanTag {
            list: BTreeMap::from([("root".to_owned(), "0".repeat(39) + "1")]),
            annotation: None,
        },
    );
    let view = compose_view(&plan).expect("the branch is still served");
    assert_eq!(git(&view, &["rev-parse", "main"]), head);
    let tags = git(&view, &["for-each-ref", "refs/tags"]);
    assert!(tags.is_empty(), "{tags}");
}
