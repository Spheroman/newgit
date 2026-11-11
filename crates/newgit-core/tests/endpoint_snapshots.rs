//! The endpoint composes from root's history and each commit it pins, never
//! a child's history. These compose one project twice, from whole histories
//! and from those snapshots alone, and hold the ids to being the same; and
//! check that what needs history still gets it, in a store a shallow commit
//! cannot reach.

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, ComposeOptions, SplitOptions};
use newgit_core::endpoint::{
    AncestryCheck, AncestryPlan, IndexPlan, Plan, Source, check_ancestry, check_index, compose_view,
};

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

fn has_object(repo: &Utf8Path, id: &str) -> bool {
    Command::new("git")
        .arg(format!("--git-dir={repo}"))
        .args(["cat-file", "-e", id])
        .output()
        .expect("git runs")
        .status
        .success()
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

/// A work tree committing at dates that only go up.
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

    fn tick(&self) -> Vec<(&'static str, String)> {
        self.time.set(self.time.get() + 100);
        who("ana", self.time.get())
    }

    fn commit(&self, files: &[(&str, &str)], message: &str) -> String {
        for (path, contents) in files {
            let path = self.dir.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, contents).expect("write");
        }
        git(&self.dir, &["add", "-A"]);
        git_env(
            &self.dir,
            &["commit", "-q", "--allow-empty", "-m", message],
            &self.tick(),
        );
        git(&self.dir, &["rev-parse", "HEAD"])
    }

    fn merge(&self, other: &str, message: &str) -> String {
        git_env(
            &self.dir,
            &["merge", "-q", "--no-ff", "-m", message, other],
            &self.tick(),
        );
        git(&self.dir, &["rev-parse", "HEAD"])
    }

    fn split(&self, tip: &str) -> compose::SplitOutcome {
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
        .unwrap_or_else(|error| panic!("split {tip}: {error}"))
    }

    /// One root commit moving `tracker`'s pin: what `newgit pull` makes.
    fn move_pin(&self, parent: &str, tracker: &str, pin: &str, message: &str) -> String {
        let index = self.dir.join(".git/pull-index");
        let env = vec![("GIT_INDEX_FILE", index.to_string())];
        git_env(&self.dir, &["read-tree", parent], &env);
        git_env(
            &self.dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{pin},.newgit/pins/{tracker}"),
            ],
            &env,
        );
        let tree = git_env(&self.dir, &["write-tree"], &env);
        let commit = git_env(
            &self.dir,
            &["commit-tree", &tree, "-p", parent, "-m", message],
            &self.tick(),
        );
        let _ = std::fs::remove_file(&index);
        commit
    }
}

struct Published {
    /// Tracker → its bare repo, as the remote holds it.
    repos: BTreeMap<String, Utf8PathBuf>,
    /// The index of `main`.
    list: BTreeMap<String, String>,
    /// The view of `main`, for a reader of everything.
    view: String,
    /// A commit of `t3code` no root commit pins.
    unpinned: String,
    /// `t3code`'s pin before the pull, and after it.
    before_pull: String,
    pulled: String,
}

/// `tracker`'s repo on the remote, with `tip` as its `main`.
fn publish(from: &Utf8Path, temp: &Utf8Path, tracker: &str, tip: &str) -> Utf8PathBuf {
    let bare = temp.join(format!("remote/{tracker}.git"));
    git(temp, &["init", "-q", "--bare", "-b", "main", bare.as_str()]);
    git(
        from,
        &[
            "push",
            "-q",
            bare.as_str(),
            &format!("{tip}:refs/heads/main"),
        ],
    );
    bare
}

/// A project through everything a history can hold: a tracker mounted, a
/// mount inside it, a pull of an outside history with a merge in it, a commit
/// that changes only files under a mount, one only under a nested mount, and
/// a merge of two lines of work.
fn project(temp: &Utf8Path) -> Published {
    let repo = Repo::new(temp.join("project"));
    let first = repo.commit(
        &[("README.md", "hello\n"), ("apps/app/main.js", "one\n")],
        "start",
    );
    git(
        &repo.dir,
        &["update-ref", "refs/newgit/trackers/root/main", &first],
    );
    repo.commit(
        &[
            (".newgit/trackers/root.toml", ROOT_TOML),
            (".newgit/trackers/access.toml", ACCESS_TOML),
        ],
        "commit the definitions",
    );
    repo.commit(
        &[(".newgit/trackers/app.toml", &mounted("apps/app"))],
        "Create tracker `app` at apps/app",
    );

    // Nested: `lib` is mounted inside `app`.
    repo.commit(
        &[
            ("apps/app/.newgit/trackers/lib.toml", &mounted("lib")),
            ("apps/app/lib/index.js", "lib\n"),
        ],
        "app mounts lib",
    );

    // A mount.
    let mount = repo.commit(
        &[
            (".newgit/trackers/t3code.toml", &mounted("apps/t3code")),
            ("apps/t3code/app.js", "one\n"),
        ],
        "mount t3code",
    );
    let split = repo.split(&mount);

    // A pull: a history made elsewhere, with a merge in it, one pin move here.
    let outside = Repo::new(temp.join("t3code"));
    git(
        &outside.dir,
        &[
            "fetch",
            "-q",
            repo.dir.as_str(),
            &format!("+{}:refs/heads/ours", split.index["t3code"]),
        ],
    );
    git(&outside.dir, &["reset", "-q", "--hard", "ours"]);
    let before_pull = git(&outside.dir, &["rev-parse", "HEAD"]);
    outside.time.set(50_000);
    git(&outside.dir, &["checkout", "-q", "-b", "side"]);
    let unpinned = outside.commit(&[("side.js", "side\n")], "t3code: side");
    git(&outside.dir, &["checkout", "-q", "main"]);
    outside.commit(&[("app.js", "one\ntwo\n")], "t3code: two");
    let pulled = outside.merge("side", "t3code: merge side");
    git(
        &repo.dir,
        &[
            "fetch",
            "-q",
            outside.dir.as_str(),
            &format!("+{pulled}:refs/trackers/t3code/main"),
        ],
    );
    let pull = repo.move_pin(&split.root, "t3code", &pulled, "Pull t3code: 3 commits");
    git(
        &repo.dir,
        &["update-ref", "refs/newgit/trackers/root/pull", &pull],
    );
    let view = compose_here(&repo.dir, &pull, &["*"]).expect("a view");
    git(&repo.dir, &["reset", "-q", "--hard", &view]);

    // A commit under the mount and nothing else: the child holds the work.
    repo.commit(&[("apps/t3code/app.js", "one\ntwo\nours\n")], "inline work");
    // The same two levels down.
    repo.commit(
        &[("apps/app/lib/index.js", "lib two\n")],
        "edit the library",
    );
    let fork = git(&repo.dir, &["rev-parse", "HEAD"]);

    // A merge of two lines of work, each in a child.
    git(&repo.dir, &["checkout", "-q", "-b", "side"]);
    repo.commit(&[("apps/app/main.js", "one\nside\n")], "side work");
    git(&repo.dir, &["checkout", "-q", "main"]);
    git(&repo.dir, &["reset", "-q", "--hard", &fork]);
    repo.commit(&[("apps/t3code/main-line.js", "main\n")], "main work");
    let merged = repo.merge("side", "Merge the side line");

    let split = repo.split(&merged);
    let repos = split
        .index
        .iter()
        .map(|(tracker, tip)| (tracker.clone(), publish(&repo.dir, temp, tracker, tip)))
        .collect();
    Published {
        repos,
        list: split.index,
        view: merged,
        unpinned,
        before_pull,
        pulled,
    }
}

fn compose_here(dir: &Utf8Path, root: &str, readable: &[&str]) -> Option<String> {
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

/// The view for `readable`, composed in a repository holding every tracker's
/// whole history.
fn from_whole_histories(project: &Published, readable: &[&str]) -> String {
    let (_guard, dir) = tempdir();
    git(&dir, &["init", "-q", "--bare"]);
    for (tracker, repo) in &project.repos {
        git(
            &dir,
            &[
                "fetch",
                "-q",
                repo.as_str(),
                &format!("+refs/heads/*:refs/trackers/{tracker}/*"),
            ],
        );
    }
    compose_here(&dir, &project.list["root"], readable).expect("a view")
}

fn sources(project: &Published) -> BTreeMap<String, Source> {
    project
        .repos
        .iter()
        .map(|(tracker, repo)| {
            (
                tracker.clone(),
                Source {
                    url: repo.to_string(),
                    auth: "Authorization: Basic eDpwdw==".to_owned(),
                    namespace: None,
                },
            )
        })
        .collect()
}

/// What the Worker sends for a reader of `readable`: their list, and where
/// their trackers and `access` are.
fn plan_for(project: &Published, container: &Utf8Path, readable: &[&str]) -> Plan {
    let list: BTreeMap<String, String> = project
        .list
        .iter()
        .filter(|(tracker, _)| readable.contains(&tracker.as_str()))
        .map(|(tracker, commit)| (tracker.clone(), commit.clone()))
        .collect();
    let repos = sources(project)
        .into_iter()
        .filter(|(tracker, _)| tracker == "access" || list.contains_key(tracker))
        .collect();
    Plan {
        root: "root".to_owned(),
        dir: container.to_path_buf(),
        branches: BTreeMap::from([("main".to_owned(), list)]),
        default_branch: "main".to_owned(),
        tags: BTreeMap::new(),
        repos,
    }
}

const EVERYONE: [&str; 5] = ["root", "access", "app", "app__lib", "t3code"];

#[test]
fn snapshots_compose_the_ids_whole_histories_do_for_every_reader() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let whole = from_whole_histories(&project, &EVERYONE);
    assert_eq!(whole, project.view, "the commits as they were made");

    let readers: [&[&str]; 6] = [
        &EVERYONE,
        // Can't read a child.
        &["root", "access", "app", "app__lib"],
        // Can't read a child, and so nothing mounted inside it.
        &["root", "access", "t3code"],
        // Can read a child but not what it mounts.
        &["root", "access", "app", "t3code"],
        // Can't read `access`, which says where everything is mounted.
        &["root", "app", "app__lib", "t3code"],
        &["root"],
    ];
    for readable in readers {
        let (_container_guard, container) = tempdir();
        let view = compose_view(&plan_for(&project, &container, readable)).expect("composed");
        assert_eq!(
            git(&view, &["rev-parse", "main"]),
            from_whole_histories(&project, readable),
            "reader of {readable:?}"
        );
    }
}

#[test]
fn composing_fetches_no_child_history_and_leaves_the_snapshots_shallow() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let (_container_guard, container) = tempdir();
    compose_view(&plan_for(&project, &container, &EVERYONE)).expect("composed");

    let whole = container.join("trackers.git");
    let snapshots = container.join("snapshots.git");
    assert!(snapshots.join("shallow").is_file());
    assert!(!whole.join("shallow").exists());
    // Every pin root's history holds, and the commit in between no pin holds
    // is not here.
    assert!(has_object(&snapshots, &project.before_pull));
    assert!(has_object(&snapshots, &project.pulled));
    assert!(!has_object(&snapshots, &project.unpinned));
    assert!(!has_object(
        &container.join("compose.git"),
        &project.unpinned
    ));
    let tracked = git(
        &temp,
        &[
            &format!("--git-dir={whole}"),
            "for-each-ref",
            "--format=%(refname)",
        ],
    );
    assert_eq!(tracked, "refs/trackers/root/heads/main");
}

#[test]
fn an_unchanged_branch_fetches_nothing_the_second_time() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let (_container_guard, container) = tempdir();
    let plan = plan_for(&project, &container, &EVERYONE);
    let view = compose_view(&plan).expect("composed");
    for repo in project.repos.values() {
        std::fs::rename(repo, repo.with_extension("gone")).expect("the remote goes away");
    }
    assert_eq!(compose_view(&plan).expect("served from cache"), view);
}

#[test]
fn a_commit_the_store_holds_for_another_tracker_is_not_this_trackers() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let (_container_guard, container) = tempdir();
    compose_view(&plan_for(&project, &container, &EVERYONE)).expect("composed");

    // `app`'s commit, in the store, named as `t3code`'s by a branch whose
    // reader may read `t3code` and not `app`.
    let secret = project.list["app"].clone();
    let mut plan = plan_for(&project, &container, &["root", "access", "t3code"]);
    plan.branches
        .get_mut("main")
        .expect("main")
        .insert("t3code".to_owned(), secret.clone());
    let view = compose_view(&plan).expect("served");
    let files = git(&view, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!files.contains("main.js"), "{files}");
    let snapshots = container.join("snapshots.git");
    let refs = git(
        &temp,
        &[
            &format!("--git-dir={snapshots}"),
            "for-each-ref",
            "--format=%(refname)",
        ],
    );
    assert!(
        refs.contains(&format!("refs/snapshots/app/{secret}")),
        "{refs}"
    );
    assert!(
        !refs.contains(&format!("refs/snapshots/t3code/{secret}")),
        "{refs}"
    );
}

#[test]
fn ancestry_answers_from_whole_histories_after_snapshots() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let (_container_guard, container) = tempdir();
    compose_view(&plan_for(&project, &container, &EVERYONE)).expect("composed");

    let check = |old: &str, new: &str| AncestryCheck {
        tracker: "t3code".to_owned(),
        old: old.to_owned(),
        new: new.to_owned(),
    };
    let answers = check_ancestry(&AncestryPlan {
        dir: container.clone(),
        repos: sources(&project),
        checks: vec![
            // Both are shallow in the snapshots, which would call this
            // unrelated.
            check(&project.before_pull, &project.pulled),
            check(&project.pulled, &project.before_pull),
        ],
    })
    .expect("checked");
    assert!(answers[0].forward, "forward along the whole history");
    assert!(answers[0].on_main);
    assert!(!answers[1].forward);
    assert!(!container.join("trackers.git/shallow").exists());
}

#[test]
fn an_index_is_checked_from_the_pins_of_one_commit() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let (_container_guard, container) = tempdir();
    let answer = check_index(&IndexPlan {
        dir: container.clone(),
        repos: sources(&project),
        list: project.list.clone(),
    })
    .expect("checked");
    assert!(answer.matches, "{:?}", answer.actual);
    assert_eq!(answer.actual, project.list);
    // Nothing of an older pin.
    assert!(!has_object(
        &container.join("snapshots.git"),
        &project.before_pull
    ));
}

#[test]
fn a_commit_a_child_shares_with_root_does_not_cut_roots_history() {
    let (_guard, temp) = tempdir();
    let repo = Repo::new(temp.join("project"));
    repo.commit(
        &[
            ("README.md", "hello\n"),
            (".newgit/trackers/root.toml", ROOT_TOML),
        ],
        "start",
    );
    let shared = repo.commit(&[("README.md", "two\n")], "shared");
    let tip = repo.commit(
        &[(".newgit/trackers/old.toml", &mounted("old"))],
        "mount a copy of an older self",
    );
    let root = repo.move_pin(&tip, "old", &shared, "Pin it");

    let list = BTreeMap::from([
        ("old".to_owned(), shared.clone()),
        ("root".to_owned(), root.clone()),
    ]);
    let published = Published {
        repos: list
            .iter()
            .map(|(tracker, tip)| (tracker.clone(), publish(&repo.dir, &temp, tracker, tip)))
            .collect(),
        list,
        view: String::new(),
        unpinned: String::new(),
        before_pull: String::new(),
        pulled: String::new(),
    };
    let (_container_guard, container) = tempdir();
    let view = compose_view(&plan_for(&published, &container, &["root", "old"])).expect("composed");
    assert_eq!(
        git(&view, &["rev-parse", "main"]),
        from_whole_histories(&published, &["root", "old"]),
    );
    assert_eq!(git(&view, &["rev-list", "--count", "main"]), "4");
}

#[test]
fn a_reader_of_a_child_but_not_what_it_mounts_gets_the_commits_own_message() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let (_container_guard, container) = tempdir();
    let readable = ["root", "access", "app", "t3code"];
    let view = compose_view(&plan_for(&project, &container, &readable)).expect("composed");
    let log = git(&view, &["log", "--format=%s", "main"]);
    assert!(log.contains("edit the library"), "{log}");
    assert!(!log.contains("Update "), "newgit's own words: {log}");
    let files = git(&view, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!files.contains("apps/app/lib/"), "{files}");
}
