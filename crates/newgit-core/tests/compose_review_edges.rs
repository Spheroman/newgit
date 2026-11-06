//! Edges of compose and split on pins found in review (#323): a merge's own
//! words keep to one audience, a pin its tracker does not have leaves the
//! tracker out instead of failing the view, a pull is one commit whoever made
//! for split work because its committer line matches, and a tracker mounted
//! at `.` loses files when its replayed history does.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, ComposeOptions, SplitOptions};

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

fn mounted(mount: &str) -> String {
    format!(
        "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\nmount = \"{mount}\"\npaths = [\".\"]\n"
    )
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A project repo: the view, where work is committed, split, and composed.
struct Repo {
    dir: Utf8PathBuf,
    time: std::cell::Cell<i64>,
}

impl Repo {
    fn new(dir: Utf8PathBuf) -> Self {
        std::fs::create_dir_all(&dir).expect("mkdir");
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "commit.gpgSign", "false"]);
        Repo {
            dir,
            time: std::cell::Cell::new(1_000),
        }
    }

    /// Write `files` (`None` deletes) and commit everything as a view
    /// commit.
    fn commit(&self, files: &[(&str, Option<&str>)], message: &str) -> String {
        for (path, contents) in files {
            let path = self.dir.join(path);
            match contents {
                Some(contents) => {
                    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                    std::fs::write(&path, contents).expect("write");
                }
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        git(&self.dir, &["add", "-A"]);
        self.time.set(self.time.get() + 100);
        git_env(
            &self.dir,
            &["commit", "-q", "--allow-empty", "-m", message],
            &who("ana", self.time.get()),
        );
        git(&self.dir, &["rev-parse", "HEAD"])
    }

    fn merge(&self, other: &str, message: &str) -> String {
        self.time.set(self.time.get() + 100);
        git_env(
            &self.dir,
            &["merge", "-q", "--no-ff", "-m", message, other],
            &who("ana", self.time.get()),
        );
        git(&self.dir, &["rev-parse", "HEAD"])
    }

    fn split(&self, tip: &str) -> compose::SplitOutcome {
        self.split_bound(tip, &[])
    }

    fn split_bound(&self, tip: &str, dropped: &[&str]) -> compose::SplitOutcome {
        let bound = |tracker: &str| !dropped.contains(&tracker);
        let audience = |_: &str| None;
        compose::split(
            &self.dir,
            tip,
            &SplitOptions {
                root: "root",
                bound: &bound,
                audience: &audience,
                newgit_made: None,
                need_history: &|_| Ok(()),
            },
        )
        .unwrap_or_else(|error| panic!("split {tip}: {error}"))
    }

    fn try_split(&self, tip: &str) -> Result<compose::SplitOutcome, String> {
        let bound = |_: &str| true;
        let audience = |_: &str| None;
        compose::split(
            &self.dir,
            tip,
            &SplitOptions {
                root: "root",
                bound: &bound,
                audience: &audience,
                newgit_made: None,
                need_history: &|_| Ok(()),
            },
        )
        .map_err(|error| error.to_string())
    }
}

/// A fresh bare repository holding only the tracker commits `refs` name —
/// what the remote has — composed from `root` for a reader of `readable`.
fn compose_fresh(
    from: &Utf8Path,
    refs: &BTreeMap<String, String>,
    root: &str,
    readable: &[&str],
) -> (tempfile::TempDir, Utf8PathBuf, Option<String>) {
    let (guard, dir) = tempdir();
    git(&dir, &["init", "-q", "--bare"]);
    for (name, commit) in refs {
        git(
            from,
            &["update-ref", &format!("refs/export/{name}"), commit],
        );
    }
    git(
        &dir,
        &[
            "fetch",
            "-q",
            from.as_str(),
            "+refs/export/*:refs/trackers/*",
        ],
    );
    let tip = compose_in(&dir, root, readable);
    (guard, dir, tip)
}

fn compose_in(dir: &Utf8Path, root: &str, readable: &[&str]) -> Option<String> {
    let all = readable.contains(&"*");
    let may = |tracker: &str| all || readable.contains(&tracker);
    compose::compose(
        dir,
        "root",
        &[("refs/heads/view".to_owned(), root.to_owned())],
        &ComposeOptions {
            readable: &may,
            admit: None,
        },
    )
    .expect("compose")
    .remove(0)
    .tip
}

fn tracker_refs(index: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    index
        .iter()
        .map(|(tracker, commit)| (format!("{tracker}/tip"), commit.clone()))
        .collect()
}

fn files(dir: &Utf8Path, commit: &str) -> Vec<String> {
    git(dir, &["ls-tree", "-r", "--name-only", commit])
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

/// A project with root and access carved, and an app mounted at `apps/app`.
fn carved_project(dir: &Utf8Path) -> (Repo, Vec<String>) {
    let repo = Repo::new(dir.join("project"));
    let first = repo.commit(
        &[
            ("README.md", Some("hello\n")),
            ("apps/app/main.js", Some("one\n")),
        ],
        "start",
    );
    // What `newgit init` does: the history it finds is root's.
    git(
        &repo.dir,
        &["update-ref", "refs/newgit/trackers/root/main", &first],
    );
    let defs = repo.commit(
        &[
            (".newgit/trackers/root.toml", Some(ROOT_TOML)),
            (".newgit/trackers/access.toml", Some(ACCESS_TOML)),
        ],
        "commit the definitions",
    );
    let carve = repo.commit(
        &[(".newgit/trackers/app.toml", Some(&mounted("apps/app")))],
        "Create tracker `app` at apps/app",
    );
    (repo, vec![first, defs, carve])
}

fn try_split_audience(repo: &Repo, tip: &str) -> Result<compose::SplitOutcome, String> {
    let bound = |_: &str| true;
    let audience = |_: &str| None;
    compose::split(
        &repo.dir,
        tip,
        &SplitOptions {
            root: "root",
            bound: &bound,
            audience: &audience,
            newgit_made: None,
            need_history: &|_| Ok(()),
        },
    )
    .map_err(|error| error.to_string())
}

fn public(mount: &str) -> String {
    format!(
        "audience = \"public\"\nstorage = \"local\"\ngit = true\nmount = \"{mount}\"\npaths = [\".\"]\n"
    )
}

/// Two branches that both changed root and a public `app`: their merge
/// makes a merge commit in each, so its message lands in both audiences.
fn merge_of_both(repo: &Repo) -> String {
    repo.commit(
        &[(".newgit/trackers/app.toml", Some(&public("apps/app")))],
        "app is public",
    );
    git(&repo.dir, &["checkout", "-q", "-b", "side"]);
    repo.commit(&[("apps/app/side.js", Some("side\n"))], "side: app");
    repo.commit(&[("SIDE.md", Some("side\n"))], "side: root");
    git(&repo.dir, &["checkout", "-q", "main"]);
    repo.commit(&[("apps/app/main.js", Some("main\n"))], "main");
    git(&repo.dir, &["rev-parse", "side"])
}

#[test]
fn a_merge_with_words_of_its_own_keeps_to_one_audience() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    merge_of_both(&repo);
    let written = repo.merge("side", "merge with private details");
    let refused = try_split_audience(&repo, &written).expect_err("mixed audiences");
    assert!(refused.contains("different audiences"), "{refused}");
    assert!(refused.contains("`app` (public)"), "{refused}");
}

#[test]
fn a_merge_in_gits_own_words_is_split_whatever_the_audiences() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    merge_of_both(&repo);
    let stock = repo.merge("side", "Merge branch 'side'");
    let outcome = try_split_audience(&repo, &stock).expect("a stock merge");
    let (_fresh_guard, _fresh, composed) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["*"],
    );
    assert_eq!(composed.as_deref(), Some(stock.as_str()));
}

/// A root commit on `parent` with `files` written (definitions among them)
/// and `pins` set, not split from any view commit: what an import or a pull
/// makes.
fn root_commit(
    repo: &Repo,
    parent: &str,
    files: &[(&str, &str)],
    pins: &[(&str, &str)],
    message: &str,
    time: i64,
) -> String {
    let index = repo.dir.join(".git/root-commit-index");
    let env = vec![("GIT_INDEX_FILE", index.to_string())];
    git_env(&repo.dir, &["read-tree", parent], &env);
    for (path, contents) in files {
        let file = repo.dir.join(".git/root-commit-blob");
        std::fs::write(&file, contents).expect("write");
        let blob = git(&repo.dir, &["hash-object", "-w", file.as_str()]);
        git_env(
            &repo.dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
            &env,
        );
    }
    for (name, pin) in pins {
        git_env(
            &repo.dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{pin},.newgit/pins/{name}"),
            ],
            &env,
        );
    }
    let tree = git_env(&repo.dir, &["write-tree"], &env);
    let _ = std::fs::remove_file(&index);
    git_env(
        &repo.dir,
        &["commit-tree", &tree, "-p", parent, "-m", message],
        &who("jack", time),
    )
}

fn root_only(temp: &Utf8Path) -> (Repo, String) {
    let repo = Repo::new(temp.join("project"));
    let start = repo.commit(
        &[
            ("README.md", Some("hello\n")),
            (".newgit/trackers/root.toml", Some(ROOT_TOML)),
        ],
        "start",
    );
    (repo, start)
}

#[test]
fn a_pin_its_tracker_does_not_have_leaves_the_tracker_out_instead_of_failing() {
    let (_guard, temp) = tempdir();
    let (repo, start) = root_only(&temp);
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let pinned = root_commit(
        &repo,
        &start,
        &[(".newgit/trackers/lib.toml", &mounted("libs/lib"))],
        &[("lib", missing)],
        "mount a lib that never reached here",
        2_000,
    );
    let after = root_commit(
        &repo,
        &pinned,
        &[("NOTES.md", "after\n")],
        &[],
        "after",
        3_000,
    );
    let view = compose_in(&repo.dir, &after, &["*"]).expect("a view, not an error");
    let files = files(&repo.dir, &view);
    assert!(
        !files.iter().any(|path| path.starts_with("libs/lib")),
        "{files:?}"
    );
    assert!(files.contains(&"NOTES.md".to_owned()));
}

#[test]
fn a_pull_stays_one_commit_when_the_child_commit_shares_its_committer_line() {
    let (_guard, temp) = tempdir();
    let (repo, start) = root_only(&temp);
    let outside = temp.join("lib");
    let up = Repo::new(outside.clone());
    let first = up.commit(&[("index.js", Some("one\n"))], "lib: one");
    // Made the same second, by the same person, as the pull that brings it.
    std::fs::write(outside.join("index.js"), "two\n").expect("write");
    git(&outside, &["add", "-A"]);
    git_env(
        &outside,
        &["commit", "-q", "-m", "lib: two"],
        &who("jack", 9_000),
    );
    let second = git(&outside, &["rev-parse", "HEAD"]);
    git(
        &repo.dir,
        &[
            "fetch",
            "-q",
            outside.as_str(),
            "+refs/heads/main:refs/trackers/lib/main",
        ],
    );
    let mounted_at = root_commit(
        &repo,
        &start,
        &[(".newgit/trackers/lib.toml", &mounted("libs/lib"))],
        &[("lib", &first)],
        "Import lib",
        5_000,
    );
    let pulled = root_commit(
        &repo,
        &mounted_at,
        &[],
        &[("lib", &second)],
        "Pull lib: 1 commit from main",
        9_000,
    );
    let view = compose_in(&repo.dir, &pulled, &["*"]).expect("view");
    assert_eq!(
        git(&repo.dir, &["log", "-1", "--format=%s %P", &view]),
        format!(
            "Pull lib: 1 commit from main {}",
            compose_in(&repo.dir, &mounted_at, &["*"]).expect("view")
        ),
        "the pull is its own commit, on one parent"
    );
}

#[test]
fn a_tracker_mounted_at_the_root_loses_a_file_a_pull_deletes() {
    let (_guard, temp) = tempdir();
    let (repo, start) = root_only(&temp);
    let outside = temp.join("kit");
    let up = Repo::new(outside.clone());
    let first = up.commit(
        &[("kit/a.js", Some("a\n")), ("kit/b.js", Some("b\n"))],
        "kit: two files",
    );
    let second = up.commit(&[("kit/a.js", None)], "kit: drop a");
    git(
        &repo.dir,
        &[
            "fetch",
            "-q",
            outside.as_str(),
            "+refs/heads/main:refs/trackers/kit/main",
        ],
    );
    let kit = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\npaths = [\"kit\"]\n";
    let mounted_at = root_commit(
        &repo,
        &start,
        &[(".newgit/trackers/kit.toml", kit)],
        &[("kit", &first)],
        "Import kit",
        5_000,
    );
    let pulled = root_commit(
        &repo,
        &mounted_at,
        &[],
        &[("kit", &second)],
        "Pull kit",
        6_000,
    );
    let view = compose_in(&repo.dir, &pulled, &["*"]).expect("view");
    let files = files(&repo.dir, &view);
    assert!(!files.contains(&"kit/a.js".to_owned()), "{files:?}");
    assert!(files.contains(&"kit/b.js".to_owned()), "{files:?}");
}
