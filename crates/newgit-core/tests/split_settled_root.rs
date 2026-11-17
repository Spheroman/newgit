//! Two root commits naming one view commit, and no note to say which: the
//! split takes the one root's `main` holds (*One commit, one root commit* in
//! docs/design/trackers.md).
//!
//! A copy that split a view commit the remote had split already, before it
//! could read the remote's, holds a root commit of its own naming it beside
//! the remote's. Which one a view commit stands for then decides every commit
//! built on it, and a mirror sees the copy's as new commits. Ordered by date
//! alone, whichever was older won.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, SplitOptions};

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

const ROOT_TOML: &str =
    "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\npaths = [\".\"]\n";
const ACCESS_TOML: &str = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\nmount = \".newgit/trackers\"\npaths = [\".\"]\nmaintainers = \"owner\"\n";

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// Commit everything in `dir` as a view commit at `time`.
fn commit(dir: &Utf8Path, files: &[(&str, &str)], message: &str, time: i64) -> String {
    for (path, contents) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
    }
    git(dir, &["add", "-A"]);
    git_env(dir, &["commit", "-q", "-m", message], &who("ana", time));
    git(dir, &["rev-parse", "HEAD"])
}

fn split(dir: &Utf8Path, tip: &str, settled: &[String]) -> compose::SplitOutcome {
    let bound = |_: &str| true;
    let audience = |_: &str| None;
    compose::split(
        dir,
        tip,
        &SplitOptions {
            root: "root",
            bound: &bound,
            audience: &audience,
            newgit_made: None,
            need_history: &|_| Ok(()),
            forks_before: &[],
            settled,
        },
    )
    .unwrap_or_else(|error| panic!("split {tip}: {error}"))
}

/// Of two root commits naming one view commit, the settled one is it,
/// whichever way their ids sort: so an order by id alone, whether its first
/// or its last candidate wins, fails one of the two tests below.
fn the_settled_one_is_it(settled_sorts_later: bool) {
    let (_guard, temp) = tempdir();
    let dir = temp.join("project");
    std::fs::create_dir_all(&dir).expect("mkdir");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "commit.gpgSign", "false"]);
    let first = commit(&dir, &[("README.md", "hello\n")], "start", 1_000);
    git(
        &dir,
        &["update-ref", "refs/newgit/trackers/root/main", &first],
    );
    commit(
        &dir,
        &[
            (".newgit/trackers/root.toml", ROOT_TOML),
            (".newgit/trackers/access.toml", ACCESS_TOML),
        ],
        "commit the definitions",
        1_100,
    );
    let shared = commit(&dir, &[("README.md", "shared\n")], "shared work", 1_200);

    // The remote's root commit for it, on root's `main`.
    let theirs = split(&dir, &shared, &[]).index["root"].clone();
    git(
        &dir,
        &["update-ref", "refs/newgit/trackers/root/main", &theirs],
    );
    assert!(compose::read_root(&dir, &shared).expect("notes").is_some());

    // A copy's, on an instance's branch: the same view commit, on the same
    // parent, but not the same root commit (it pinned something else), and
    // older, as a copy split before it heard of the remote's is.
    let parent = git(&dir, &["rev-parse", &format!("{theirs}^")]);
    let message = git(&dir, &["log", "-1", "--format=%B", &theirs]);
    let tree = git(&dir, &["rev-parse", &format!("{theirs}^{{tree}}")]);
    let blob_file = temp.join("extra");
    std::fs::write(&blob_file, "only the copy has this\n").expect("write");
    let blob = git(&dir, &["hash-object", "-w", blob_file.as_str()]);
    let index = temp.join("copy-index");
    let env = vec![("GIT_INDEX_FILE", index.to_string())];
    git_env(&dir, &["read-tree", &tree], &env);
    git_env(
        &dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},COPY.md"),
        ],
        &env,
    );
    let copy_tree = git_env(&dir, &["write-tree"], &env);
    // Its date picks its id: tried until the two sort the way this run asks.
    let copy = (1_150..1_190)
        .map(|time| {
            let mut copy_env = who("ana", time);
            copy_env.push(("GIT_INDEX_FILE", index.to_string()));
            git_env(
                &dir,
                &["commit-tree", &copy_tree, "-p", &parent, "-m", &message],
                &copy_env,
            )
        })
        .find(|copy| (*copy < theirs) == settled_sorts_later)
        .expect("a copy whose id sorts as asked");
    assert_ne!(copy, theirs);
    git(
        &dir,
        &[
            "update-ref",
            "refs/newgit/trackers/root/instances/copy",
            &copy,
        ],
    );

    // No note says which: the copy's split never wrote one the remote's
    // would agree with, and composing `main` here, which would, failed.
    git(&dir, &["update-ref", "-d", "refs/notes/newgit-composed"]);

    let next = commit(&dir, &[("NEXT.md", "next\n")], "next", 1_300);
    let settled = vec![theirs.clone()];
    let root = split(&dir, &next, &settled).index["root"].clone();
    assert_eq!(
        git(&dir, &["rev-parse", &format!("{root}^")]),
        theirs,
        "built on the copy's root commit, {copy}:\n{}",
        git(&dir, &["log", "--format=%h %p %s", "-3", &root])
    );
}

#[test]
fn the_settled_root_commit_wins_when_its_id_sorts_later() {
    the_settled_one_is_it(true);
}

#[test]
fn the_settled_root_commit_wins_when_its_id_sorts_earlier() {
    the_settled_one_is_it(false);
}
