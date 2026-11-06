//! Compose and split on pins (docs/design/trackers.md, *Compose* and
//! *Split*), at the engine: view commits split into root and the trackers
//! they change, then composed again — here, and in a fresh repository
//! holding only tracker commits, the remote's situation — to the very same
//! ids.

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

#[test]
fn split_view_commits_compose_back_to_themselves_anywhere() {
    let (_guard, temp) = tempdir();
    let (repo, history) = carved_project(&temp);
    let work = repo.commit(
        &[
            ("README.md", Some("hello, world\n")),
            ("apps/app/main.js", Some("one\ntwo\n")),
        ],
        "touch both",
    );
    let outcome = repo.split(&work);
    // One root commit per view commit, the app's work in its own history at
    // its own root.
    assert_eq!(
        git(&repo.dir, &["rev-list", "--count", &outcome.root]),
        "4",
        "root's history is the view's shape: one root commit per view commit"
    );
    let app = &outcome.index["app"];
    assert_eq!(files(&repo.dir, app), vec!["main.js"]);
    assert_eq!(
        git(&repo.dir, &["log", "--format=%s", app]),
        "touch both\nCreate tracker `app` at apps/app",
        "the app's history starts at the carve and keeps the message without trailers"
    );
    assert!(
        !files(&repo.dir, &outcome.root)
            .iter()
            .any(|path| path.starts_with("apps/app")),
        "root's own tree leaves the mount out"
    );
    assert!(files(&repo.dir, &outcome.root).contains(&".newgit/pins/app".to_owned()));
    // History before the carve stays in root, unchanged.
    for commit in &history[..1] {
        assert_eq!(
            compose::read_root(&repo.dir, commit).ok().flatten(),
            None,
            "nothing split for commits that already are root's"
        );
    }

    let (_fresh_guard, fresh, tip) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["*"],
    );
    assert_eq!(
        tip.as_deref(),
        Some(work.as_str()),
        "the same id in a fresh repository"
    );
    assert_eq!(
        git(&fresh, &["rev-list", "--count", "refs/heads/view"]),
        "4"
    );
    // And here.
    assert_eq!(
        compose_in(&repo.dir, &outcome.root, &["*"]).as_deref(),
        Some(work.as_str())
    );
}

#[test]
fn a_merge_inside_split_work_round_trips_exactly() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    let base = git(&repo.dir, &["rev-parse", "HEAD"]);
    git(&repo.dir, &["checkout", "-q", "-b", "side"]);
    let side = repo.commit(&[("apps/app/side.js", Some("side\n"))], "side work");
    git(&repo.dir, &["checkout", "-q", "main"]);
    repo.commit(&[("README.md", Some("main moved\n"))], "main work");
    let merge = repo.merge(&side, "Merge side");
    let outcome = repo.split(&merge);
    assert_eq!(
        git(
            &repo.dir,
            &["rev-list", "--count", "--merges", &outcome.root]
        ),
        "1",
        "a merge in the view is a merge in root"
    );
    let (_fresh_guard, fresh, tip) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["*"],
    );
    assert_eq!(tip.as_deref(), Some(merge.as_str()));
    assert_eq!(
        git(&fresh, &["rev-parse", "refs/heads/view^2"]),
        side,
        "the merged branch keeps its id"
    );
    let _ = base;
}

#[test]
fn a_commit_touching_two_trackers_stays_one_commit_merged_into_a_moved_main() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    git(&repo.dir, &["checkout", "-q", "-b", "instance"]);
    let both = repo.commit(
        &[
            ("README.md", Some("instance\n")),
            ("apps/app/main.js", Some("instance\n")),
        ],
        "one commit, two trackers",
    );
    git(&repo.dir, &["checkout", "-q", "main"]);
    repo.commit(&[("NOTES.md", Some("main moved\n"))], "main moved");
    let merge = repo.merge("instance", "Merge `instance` into main");
    let outcome = repo.split(&merge);
    let (_fresh_guard, fresh, tip) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["*"],
    );
    assert_eq!(tip.as_deref(), Some(merge.as_str()));
    let commits = git(&fresh, &["log", "--format=%H %s", "refs/heads/view"]);
    assert_eq!(
        commits
            .lines()
            .filter(|line| line.ends_with("one commit, two trackers"))
            .collect::<Vec<_>>(),
        vec![format!("{both} one commit, two trackers")],
        "one commit, with its own id: {commits}"
    );
}

#[test]
fn a_criss_cross_merge_and_a_diamond_round_trip() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    git(&repo.dir, &["checkout", "-q", "-b", "a"]);
    let a1 = repo.commit(&[("a.txt", Some("a1\n"))], "a1");
    git(&repo.dir, &["checkout", "-q", "-b", "b", "main"]);
    let b1 = repo.commit(&[("apps/app/b.js", Some("b1\n"))], "b1");
    // Criss-cross: each merges the other.
    git(&repo.dir, &["checkout", "-q", "a"]);
    let a2 = repo.merge(&b1, "a takes b");
    git(&repo.dir, &["checkout", "-q", "b"]);
    let b2 = repo.merge(&a1, "b takes a");
    git(&repo.dir, &["checkout", "-q", "a"]);
    let a3 = repo.commit(&[("a.txt", Some("a3\n"))], "a3");
    git(&repo.dir, &["checkout", "-q", "b"]);
    let b3 = repo.commit(&[("apps/app/b.js", Some("b3\n"))], "b3");
    git(&repo.dir, &["checkout", "-q", "a"]);
    let tip = repo.merge(&b3, "criss-cross");
    let outcome = repo.split(&tip);
    let (_fresh_guard, _fresh, composed) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["*"],
    );
    assert_eq!(composed.as_deref(), Some(tip.as_str()));
    let _ = (a2, b2, a3);
}

#[test]
fn a_reader_missing_a_child_gets_everything_else() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    repo.commit(&[("apps/app/main.js", Some("app only\n"))], "app only");
    let tip = repo.commit(&[("README.md", Some("after\n"))], "readme after");
    let outcome = repo.split(&tip);
    let (_fresh_guard, fresh, composed) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["root", "access"],
    );
    let composed = composed.expect("a view");
    assert!(
        !files(&fresh, &composed)
            .iter()
            .any(|path| path.starts_with("apps/app")),
        "the child is left out of every tree"
    );
    let subjects = git(&fresh, &["log", "--format=%s", &composed]);
    assert!(
        !subjects.contains("app only"),
        "a commit that only changed it is dropped: {subjects}"
    );
    assert!(subjects.contains("readme after"), "{subjects}");
    assert_ne!(
        composed, tip,
        "and ids differ from the first commit that touched it"
    );
}

#[test]
fn nested_mounts_compose_and_split_through_the_child() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    // The app mounts a library of its own, defined in its own definitions.
    let nested = repo.commit(
        &[
            ("apps/app/.newgit/trackers/lib.toml", Some(&mounted("lib"))),
            ("apps/app/lib/index.js", Some("lib\n")),
        ],
        "app mounts lib",
    );
    let outcome = repo.split(&nested);
    let lib = &outcome.index["app__lib"];
    assert_eq!(files(&repo.dir, lib), vec!["index.js"]);
    let app_files = files(&repo.dir, &outcome.index["app"]);
    assert!(
        app_files.contains(&".newgit/pins/lib".to_owned()),
        "{app_files:?}"
    );
    assert!(!app_files.contains(&"lib/index.js".to_owned()));
    let edit = repo.commit(
        &[("apps/app/lib/index.js", Some("lib two\n"))],
        "edit the library",
    );
    let outcome = repo.split(&edit);
    let (_fresh_guard, _fresh, composed) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["*"],
    );
    assert_eq!(composed.as_deref(), Some(edit.as_str()));
}

/// A name is local to its parent, as a file's is to its directory: two
/// parents with a `ui` have two trackers, each at its own version.
#[test]
fn two_parents_with_a_tracker_of_one_name_have_two_trackers() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    let both = repo.commit(
        &[
            (".newgit/trackers/ui.toml", Some(&mounted("ui"))),
            ("ui/button.js", Some("v2\n")),
            (
                "apps/app/.newgit/trackers/ui.toml",
                Some(&mounted("vendor/ui")),
            ),
            ("apps/app/vendor/ui/button.js", Some("v1\n")),
        ],
        "one ui each",
    );
    let outcome = repo.split(&both);
    assert_eq!(
        git(
            &repo.dir,
            &["show", &format!("{}:button.js", outcome.index["ui"])]
        ),
        "v2"
    );
    assert_eq!(
        git(
            &repo.dir,
            &["show", &format!("{}:button.js", outcome.index["app__ui"])]
        ),
        "v1"
    );
}

#[test]
fn signed_and_non_utf8_commits_come_back_as_themselves_from_tracker_commits_alone() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    let head = git(&repo.dir, &["rev-parse", "HEAD"]);
    let tree = git(&repo.dir, &["rev-parse", "HEAD^{tree}"]);
    // A commit as a forge signs it, with a Latin-1 author and message, made
    // byte for byte.
    let raw = format!(
        "tree {tree}\nparent {head}\nauthor B\u{e9}a <bea@example.com> 5000 +0000\ncommitter GitHub <noreply@github.com> 5000 +0000\nencoding ISO-8859-1\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n wsFcBAABCAAQ\r\n -----END PGP SIGNATURE-----\n\ncaf\u{e9} au lait"
    );
    // Latin-1: é is one byte, 0xe9.
    let bytes: Vec<u8> = raw
        .chars()
        .map(|character| character as u32 as u8)
        .collect();
    let path = temp.join("raw-commit");
    std::fs::write(&path, &bytes).expect("write");
    let signed = git(
        &repo.dir,
        &[
            "hash-object",
            "--literally",
            "-t",
            "commit",
            "-w",
            path.as_str(),
        ],
    );
    git(&repo.dir, &["update-ref", "refs/heads/main", &signed]);
    let then = repo.commit(
        &[("apps/app/main.js", Some("after\n"))],
        "after the signed one",
    );
    let outcome = repo.split(&then);
    let (_fresh_guard, fresh, composed) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["*"],
    );
    assert_eq!(composed.as_deref(), Some(then.as_str()));
    assert_eq!(git(&fresh, &["rev-parse", "refs/heads/view^"]), signed);
    let author = Command::new("git")
        .arg("-C")
        .arg(fresh.as_str())
        .args(["cat-file", "commit", &signed])
        .output()
        .expect("git")
        .stdout;
    assert!(
        author.windows(4).any(|window| window == b"B\xe9a "),
        "the Latin-1 author survives"
    );
}

// ── A history brought in from outside (#139's acceptance cases) ─────────

/// The project at `main` with an outside repository's history (files at its
/// own root, older than everything, with a merge) fetched unchanged as
/// tracker `t3code`. Returns the upstream tip.
fn upstream(repo: &Repo) -> (String, Vec<String>) {
    let outside = repo.dir.with_file_name("t3code");
    let up = Repo::new(outside.clone());
    up.time.set(10);
    let x1 = up.commit(&[("app.js", Some("one\n"))], "t3code: start");
    git(&outside, &["checkout", "-q", "-b", "side"]);
    let side = up.commit(&[("side.js", Some("side\n"))], "t3code: side");
    git(&outside, &["checkout", "-q", "main"]);
    let x2 = up.commit(&[("app.js", Some("one\ntwo\n"))], "t3code: two");
    let tip = up.merge("side", "t3code: merge side");
    git(
        &repo.dir,
        &[
            "fetch",
            "-q",
            outside.as_str(),
            "+refs/heads/main:refs/trackers/t3code/main",
        ],
    );
    (tip.clone(), vec![x1, side, x2, tip])
}

/// One root commit adding the mount and pin: what an import makes. Not
/// split from a view commit, so it brings the history in.
fn import(repo: &Repo, parent_root: &str, upstream_tip: &str) -> String {
    let index = repo.dir.join(".git/import-index");
    let env = vec![("GIT_INDEX_FILE", index.to_string())];
    git_env(&repo.dir, &["read-tree", parent_root], &env);
    let blob = {
        let path = repo.dir.join(".git/t3code.toml");
        std::fs::write(&path, mounted("apps/t3code")).expect("write");
        git(&repo.dir, &["hash-object", "-w", path.as_str()])
    };
    git_env(
        &repo.dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},.newgit/trackers/t3code.toml"),
        ],
        &env,
    );
    git_env(
        &repo.dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{upstream_tip},.newgit/pins/t3code"),
        ],
        &env,
    );
    let tree = git_env(&repo.dir, &["write-tree"], &env);
    let commit = git_env(
        &repo.dir,
        &[
            "commit-tree",
            &tree,
            "-p",
            parent_root,
            "-m",
            "Import t3code",
        ],
        &who("jack", 9_000),
    );
    let _ = std::fs::remove_file(&index);
    commit
}

/// A root-only project (definitions still in root's own tree) with `a`,
/// `b` on `main`.
fn root_project(temp: &Utf8Path) -> (Repo, String, String) {
    let repo = Repo::new(temp.join("project"));
    let a = repo.commit(
        &[
            ("README.md", Some("hello\n")),
            (".newgit/trackers/root.toml", Some(ROOT_TOML)),
        ],
        "start",
    );
    let b = repo.commit(&[("README.md", Some("hello, world\n"))], "readme");
    (repo, a, b)
}

#[test]
fn an_import_joins_as_one_commit_and_keeps_every_id() {
    let (_guard, temp) = tempdir();
    let (repo, a, b) = root_project(&temp);
    let (tip, imported) = upstream(&repo);
    let joined = import(&repo, &b, &tip);
    let view = compose_in(&repo.dir, &joined, &["*"]).expect("a view");
    // One ordinary commit on top of the original history.
    assert_eq!(
        git(&repo.dir, &["rev-parse", &format!("{view}^@")]),
        b,
        "one parent: nothing of the child's history"
    );
    assert_eq!(git(&repo.dir, &["rev-parse", &format!("{b}^")]), a);
    assert_eq!(
        files(&repo.dir, &view),
        vec![
            ".newgit/trackers/root.toml",
            ".newgit/trackers/t3code.toml",
            "README.md",
            "apps/t3code/app.js",
            "apps/t3code/side.js",
        ],
    );
    // The tracker keeps the history as it came.
    assert_eq!(
        git(&repo.dir, &["rev-parse", "refs/trackers/t3code/main"]),
        imported[3]
    );
    // `git log` starts at the import.
    let log = git(
        &repo.dir,
        &["log", "--format=%s", &view, "--", "apps/t3code/app.js"],
    );
    assert_eq!(log, "Import t3code");

    // Made again from tracker commits alone, it is the same commit.
    let refs = BTreeMap::from([
        ("root/main".to_owned(), joined.clone()),
        ("t3code/main".to_owned(), tip.clone()),
    ]);
    let (_fresh_guard, _fresh, again) = compose_fresh(&repo.dir, &refs, &joined, &["*"]);
    assert_eq!(again.as_deref(), Some(view.as_str()));
}

#[test]
fn a_branch_from_before_the_import_has_no_t3code_until_it_merges_main_and_keeps_it_then() {
    let (_guard, temp) = tempdir();
    let (repo, a, b) = root_project(&temp);
    let (tip, _) = upstream(&repo);
    let joined = import(&repo, &b, &tip);
    let main_view = compose_in(&repo.dir, &joined, &["*"]).expect("main");
    git(
        &repo.dir,
        &["update-ref", "refs/newgit/trackers/root/main", &joined],
    );

    // A branch forked at `a`, with work of its own: no t3code.
    git(&repo.dir, &["checkout", "-q", "-b", "old", &a]);
    let work = repo.commit(&[("NOTES.md", Some("old work\n"))], "old work");
    let old = repo.split(&work);
    assert!(
        !files(
            &repo.dir,
            &compose_in(&repo.dir, &old.root, &["*"]).expect("view")
        )
        .iter()
        .any(|path| path.starts_with("apps/t3code")),
        "a branch that never added the mount never has t3code"
    );

    // It merges `main`: Git's own merge brings the mount in.
    git(&repo.dir, &["update-ref", "refs/heads/main", &main_view]);
    let merged = repo.merge("main", "Merge main into old");
    let outcome = repo.split(&merged);
    let view = compose_in(&repo.dir, &outcome.root, &["*"]).expect("view");
    assert_eq!(view, merged);
    assert!(files(&repo.dir, &view).contains(&"apps/t3code/app.js".to_owned()));

    // And a pre-import branch merged *into* main keeps t3code there.
    git(&repo.dir, &["checkout", "-q", "-b", "pre", &a]);
    let pre = repo.commit(&[("PRE.md", Some("pre\n"))], "pre-import work");
    repo.split(&pre);
    git(&repo.dir, &["checkout", "-q", "main"]);
    let into_main = repo.merge("pre", "Merge `pre` into main");
    let outcome = repo.split(&into_main);
    let refs = BTreeMap::from([
        ("root/main".to_owned(), outcome.root.clone()),
        ("t3code/main".to_owned(), tip.clone()),
    ]);
    let (_fresh_guard, fresh, again) = compose_fresh(&repo.dir, &refs, &outcome.root, &["*"]);
    assert_eq!(again.as_deref(), Some(into_main.as_str()));
    assert!(files(&fresh, &into_main).contains(&"apps/t3code/app.js".to_owned()));
    assert!(files(&fresh, &into_main).contains(&"PRE.md".to_owned()));
}

#[test]
fn an_upstream_sync_dated_before_everything_goes_on_top() {
    let (_guard, temp) = tempdir();
    let (repo, _a, b) = root_project(&temp);
    let (tip, _) = upstream(&repo);
    let joined = import(&repo, &b, &tip);
    let first = compose_in(&repo.dir, &joined, &["*"]).expect("main");
    git(&repo.dir, &["reset", "-q", "--hard", &first]);
    // Work in the view on both trackers, split.
    let work = repo.commit(
        &[
            ("README.md", Some("after the import\n")),
            ("apps/t3code/app.js", Some("one\ntwo\nours\n")),
        ],
        "work on both",
    );
    let split = repo.split(&work);
    // Upstream moves on with a commit dated far in the past, merged into
    // t3code's history on top of ours, as a sync does.
    let outside = repo.dir.with_file_name("t3code");
    git(
        &outside,
        &[
            "fetch",
            "-q",
            repo.dir.as_str(),
            &format!("+{}:refs/heads/ours", split.index["t3code"]),
        ],
    );
    git(&outside, &["checkout", "-q", "-b", "sync", "ours"]);
    std::fs::write(outside.join("up.js"), "upstream\n").expect("write");
    git(&outside, &["add", "-A"]);
    git_env(
        &outside,
        &["commit", "-q", "-m", "upstream: dated long ago"],
        &who("up", 5),
    );
    let synced = git(&outside, &["rev-parse", "HEAD"]);
    git(
        &repo.dir,
        &[
            "fetch",
            "-q",
            outside.as_str(),
            &format!("+{synced}:refs/trackers/t3code/main"),
        ],
    );
    // `newgit pull t3code`: one root commit moving the pin.
    let index = repo.dir.join(".git/pull-index");
    let env = vec![("GIT_INDEX_FILE", index.to_string())];
    git_env(&repo.dir, &["read-tree", &split.root], &env);
    git_env(
        &repo.dir,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{synced},.newgit/pins/t3code"),
        ],
        &env,
    );
    let tree = git_env(&repo.dir, &["write-tree"], &env);
    let pull = git_env(
        &repo.dir,
        &[
            "commit-tree",
            &tree,
            "-p",
            &split.root,
            "-m",
            "Pull t3code: 1 commit from main",
        ],
        &who("jack", 99_000),
    );
    let view = compose_in(&repo.dir, &pull, &["*"]).expect("view");
    assert_eq!(
        git(&repo.dir, &["rev-parse", &format!("{view}^@")]),
        work,
        "one commit on top of the work, whatever the upstream commit's date"
    );
    assert_eq!(
        git(
            &repo.dir,
            &["log", "--format=%s", &format!("{work}..{view}")]
        ),
        "Pull t3code: 1 commit from main",
    );
    assert!(files(&repo.dir, &view).contains(&"apps/t3code/up.js".to_owned()));
}

#[test]
fn a_diamond_of_merges_through_two_trackers_round_trips() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    let top = git(&repo.dir, &["rev-parse", "HEAD"]);
    git(&repo.dir, &["checkout", "-q", "-b", "left"]);
    repo.commit(&[("apps/app/left.js", Some("left\n"))], "left");
    git(&repo.dir, &["checkout", "-q", "-b", "right", &top]);
    repo.commit(&[("right.txt", Some("right\n"))], "right");
    git(&repo.dir, &["checkout", "-q", "left"]);
    let bottom = repo.merge("right", "join the diamond");
    let outcome = repo.split(&bottom);
    let (_fresh_guard, fresh, composed) = compose_fresh(
        &repo.dir,
        &tracker_refs(&outcome.index),
        &outcome.root,
        &["*"],
    );
    assert_eq!(composed.as_deref(), Some(bottom.as_str()));
    assert_eq!(
        git(
            &fresh,
            &["merge-base", "refs/heads/view^1", "refs/heads/view^2"]
        ),
        top,
        "the diamond's top is one commit"
    );
}

#[test]
fn an_edit_to_one_of_two_trackers_of_one_name_splits_into_that_one() {
    let (_guard, temp) = tempdir();
    let (repo, _) = carved_project(&temp);
    let first = repo.commit(
        &[
            (".newgit/trackers/ui.toml", Some(&mounted("ui"))),
            ("ui/button.js", Some("v2\n")),
        ],
        "root mounts ui",
    );
    repo.split(&first);
    let second = repo.commit(
        &[
            (
                "apps/app/.newgit/trackers/ui.toml",
                Some(&mounted("vendor/ui")),
            ),
            ("apps/app/vendor/ui/button.js", Some("v1\n")),
        ],
        "the app mounts its own copy",
    );
    let outcome = repo.split(&second);
    let edit = repo.commit(
        &[("apps/app/vendor/ui/button.js", Some("v1.1\n"))],
        "edit one copy",
    );
    let outcome = {
        let _ = outcome;
        repo.split(&edit)
    };
    let composed = compose_in(&repo.dir, &outcome.root, &["*"]);
    assert_eq!(composed.as_deref(), Some(edit.as_str()));
    assert_eq!(
        git(&repo.dir, &["show", &format!("{edit}:ui/button.js")]),
        "v2"
    );
    assert_eq!(
        git(
            &repo.dir,
            &["show", &format!("{edit}:apps/app/vendor/ui/button.js")]
        ),
        "v1.1"
    );
}
