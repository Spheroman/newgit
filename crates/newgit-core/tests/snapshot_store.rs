//! A project repo holding snapshots (`newgit_core::snapshots`): what stays
//! shallow, what does not, and what never gets cut.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::snapshots::{self, Source};
use newgit_core::source::GitSource;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path =
        Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("canonicalize")).expect("utf8");
    (dir, path)
}

/// A repository with `commits` commits on `main`, one file each.
fn history(path: &Utf8Path, commits: usize) {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "-b", "main"]);
    for n in 0..commits {
        std::fs::write(path.join("file"), format!("{n}\n")).expect("write");
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", &format!("commit {n}")]);
    }
}

fn source(url: &Utf8Path) -> Source {
    Source {
        url: url.to_string(),
        auth: "x-newgit: test".to_owned(),
    }
}

/// A commit in `repo` on `parent` whose tree pins `child` as tracker `name`.
fn pinning(repo: &Utf8Path, name: &str, child: &str) -> String {
    let entry = format!("160000 commit {child}\t.newgit/pins/{name}\n");
    let index = repo.join(".git/pinning-index");
    let run = |args: &[&str], input: Option<&str>| {
        use std::io::Write as _;
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo.as_str())
            .args(args)
            .env("GIT_INDEX_FILE", index.as_str())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let mut child = command.spawn().expect("git runs");
        if let Some(input) = input {
            child
                .stdin
                .take()
                .expect("piped")
                .write_all(input.as_bytes())
                .expect("write");
        } else {
            drop(child.stdin.take());
        }
        let output = child.wait_with_output().expect("git waits");
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    run(&["read-tree", "HEAD"], None);
    run(&["update-index", "--index-info"], Some(&entry));
    let tree = run(&["write-tree"], None);
    let commit = run(
        &["commit-tree", &tree, "-p", "HEAD", "-m", "pin a child"],
        None,
    );
    git(repo, &["update-ref", "refs/heads/main", &commit]);
    commit
}

#[test]
fn an_ordinary_fetch_leaves_snapshots_shallow_and_finishes_nothing() {
    let (_guard, temp) = tempdir();
    let child = temp.join("child");
    history(&child, 5);
    let tip = git(&child, &["rev-parse", "main"]);
    let other = temp.join("other");
    history(&other, 3);
    let project = temp.join("project");
    history(&project, 1);

    let location = ["-C", project.as_str()];
    let absent = GitSource::fetch_snapshots(
        &location,
        child.as_str(),
        "refs/newgit/snapshots/child/",
        std::slice::from_ref(&tip),
        None,
    )
    .expect("fetch");
    assert!(absent.is_empty());
    assert!(
        snapshots::held(&project)
            .expect("held")
            .contains_key("child")
    );
    assert!(!snapshots::left_shallow(&location).expect("shallow"));

    GitSource::fetch_refs(
        &location,
        other.as_str(),
        &["+refs/heads/*:refs/newgit/remote/other/*".to_owned()],
        None,
    )
    .expect("fetch");
    assert_eq!(git(&project, &["rev-list", "--count", &tip]), "1");
    assert!(
        snapshots::held(&project)
            .expect("held")
            .contains_key("child")
    );
}

#[test]
fn a_shallow_store_with_no_snapshots_is_a_stepped_fetch_to_finish() {
    let (_guard, temp) = tempdir();
    let child = temp.join("child");
    history(&child, 5);
    let url = format!("file://{child}");
    let stopped = temp.join("stopped");
    history(&stopped, 1);
    git(
        &stopped,
        &[
            "fetch",
            "-q",
            "--depth=1",
            &url,
            "+refs/heads/main:refs/remote/main",
        ],
    );
    let location = ["-C", stopped.as_str()];
    assert!(snapshots::left_shallow(&location).expect("shallow"));

    GitSource::fetch_refs(
        &location,
        &url,
        &["+refs/heads/*:refs/remote/*".to_owned()],
        None,
    )
    .expect("fetch");
    assert_eq!(
        git(&stopped, &["rev-list", "--count", "refs/remote/main"]),
        "5"
    );
    assert!(!snapshots::left_shallow(&location).expect("shallow"));
}

#[test]
fn a_pin_of_a_commit_root_already_has_is_not_cut() {
    let (_guard, temp) = tempdir();
    let root = temp.join("root");
    history(&root, 4);
    // The child's commit is one of root's own.
    let shared = git(&root, &["rev-parse", "main~1"]);
    let tip = pinning(&root, "child", &shared);
    let child = temp.join("child");
    git(&temp, &["clone", "-q", root.as_str(), child.as_str()]);

    let project = temp.join("project");
    history(&project, 1);
    git(
        &project,
        &[
            "fetch",
            "-q",
            root.as_str(),
            "+refs/heads/main:refs/newgit/trackers/root/main",
        ],
    );
    let absent = snapshots::fetch_pins(
        &project,
        std::slice::from_ref(&tip),
        &BTreeMap::new(),
        &mut |_| Ok(Some(source(&child))),
    )
    .expect("fetch");
    assert!(absent.is_empty());
    assert_eq!(
        git(&project, &["rev-list", "--count", &tip]),
        "5",
        "root's whole history"
    );
    assert_eq!(
        git(&project, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
}

#[test]
fn root_reaching_a_snapshot_is_reported() {
    let (_guard, temp) = tempdir();
    let child = temp.join("child");
    history(&child, 3);
    let tip = git(&child, &["rev-parse", "main"]);
    let project = temp.join("project");
    history(&project, 1);
    GitSource::fetch_snapshots(
        &["-C", project.as_str()],
        child.as_str(),
        "refs/newgit/snapshots/child/",
        std::slice::from_ref(&tip),
        None,
    )
    .expect("fetch");
    let held = snapshots::held(&project).expect("held");

    let main = git(&project, &["rev-parse", "main"]);
    assert!(
        snapshots::cutting_root(&project, std::slice::from_ref(&main), &held)
            .expect("walk")
            .is_empty()
    );
    assert_eq!(
        snapshots::cutting_root(&project, std::slice::from_ref(&tip), &held).expect("walk"),
        ["child"]
    );
}
