//! An imported tracker (newgit-trackers.md, *An imported tracker joins
//! through one merge*): a history from outside the project joins each view
//! through one merge, marked on its history, rather than interleaving by
//! date, so taking it rewrites nothing — on `main` at the import, and on a
//! branch from before the import when that branch takes it.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, TrackerTip};
use newgit_core::tracker::{Storage, TrackerDefinition};

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

/// Author and committer for a commit at `time`, so dates are deterministic.
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

fn commit(repo: &Utf8Path, files: &[(&str, &str)], message: &str, time: i64) -> String {
    for (path, contents) in files {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
    }
    git(repo, &["add", "-A"]);
    git_env(
        repo,
        &["commit", "-q", "--no-gpg-sign", "-m", message],
        &who("ana", time),
    );
    git(repo, &["rev-parse", "HEAD"])
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn tracker(name: &str, paths: &[&str], excluded: &[&str]) -> TrackerDefinition {
    TrackerDefinition {
        name: name.to_owned(),
        audience: "project-devs".to_owned(),
        storage: Storage::Local,
        merge_with_source: true,
        paths: paths.iter().map(Utf8PathBuf::from).collect(),
        secret: false,
        maintainers: None,
        role: None,
        priority: 0,
        requires: Vec::new(),
        resources: Vec::new(),
        excluded: excluded.iter().map(Utf8PathBuf::from).collect(),
        definition_rev: "sha256:000000000000".to_owned(),
    }
}

/// A project (root: `a`, then `b`) and, on an unrelated branch, an outside
/// history under `apps/t3code` that is older than all of it and has a
/// merge — what an import writes.
struct Project {
    repo: Utf8PathBuf,
    a: String,
    b: String,
    /// Every imported commit, and the import's tip last.
    imported: Vec<String>,
}

fn project(temp: &Utf8Path) -> Project {
    let repo = temp.join("project");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    let a = commit(&repo, &[("README.md", "hello\n")], "start", 5_000);
    let b = commit(&repo, &[("README.md", "hello, world\n")], "readme", 6_000);

    git(&repo, &["checkout", "-q", "--orphan", "t3code"]);
    git(&repo, &["rm", "-rfq", "."]);
    let x1 = commit(
        &repo,
        &[("apps/t3code/app.js", "one\n")],
        "t3code: start",
        1_000,
    );
    git(&repo, &["checkout", "-q", "-b", "t3code-side"]);
    let side = commit(
        &repo,
        &[("apps/t3code/side.js", "side\n")],
        "t3code: side",
        2_500,
    );
    git(&repo, &["checkout", "-q", "t3code"]);
    let x2 = commit(
        &repo,
        &[("apps/t3code/app.js", "one\ntwo\n")],
        "t3code: two",
        2_000,
    );
    git_env(
        &repo,
        &[
            "merge",
            "-q",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            "t3code: merge side",
            "t3code-side",
        ],
        &who("ana", 3_000),
    );
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);
    Project {
        repo,
        a,
        b,
        imported: vec![x1, side, x2, tip],
    }
}

const T3CODE_MAIN: &str = "refs/test/t3code/main";

fn definitions() -> (TrackerDefinition, TrackerDefinition) {
    (
        tracker("root", &["."], &["apps/t3code"]),
        tracker("t3code", &["apps/t3code"], &[]),
    )
}

/// The import on `main`: t3code joins after `b`.
fn import(project: &Project, t3code: &TrackerDefinition) -> compose::Joined {
    let under = project.imported.last().expect("tip");
    compose::join(&project.repo, t3code, &project.b, under, T3CODE_MAIN).expect("join")
}

fn tips<'a>(
    root: &'a TrackerDefinition,
    root_commit: &str,
    t3code: &'a TrackerDefinition,
    t3code_commit: &str,
) -> Vec<TrackerTip<'a>> {
    vec![
        TrackerTip {
            definition: root,
            commit: root_commit.to_owned(),
        },
        TrackerTip {
            definition: t3code,
            commit: t3code_commit.to_owned(),
        },
    ]
}

fn view(repo: &Utf8Path, name: &str, tips: &[TrackerTip<'_>], all: &[TrackerDefinition]) -> String {
    compose::compose(repo, &format!("refs/newgit/views/{name}"), tips, all)
        .expect("compose")
        .tip
        .expect("a view")
}

#[test]
fn an_imported_tracker_joins_main_through_one_merge() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    let (root, t3code) = definitions();
    let all = [root.clone(), t3code.clone()];
    let under = project.imported.last().expect("tip");
    let joined = import(&project, &t3code);

    let merge = view(
        repo,
        "main",
        &tips(&root, &project.b, &t3code, &joined.marker),
        &all,
    );
    assert_eq!(merge, joined.view, "the join is reused as itself");

    // One commit appended; everything before it is the original history.
    assert_eq!(git(repo, &["rev-parse", &format!("{merge}^1")]), project.b);
    assert_eq!(git(repo, &["rev-parse", &format!("{merge}^2")]), *under);
    assert_eq!(
        git(repo, &["rev-list", "--first-parent", &merge]),
        format!("{merge}\n{}\n{}", project.b, project.a),
        "the project's commits keep their ids"
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%an <%ae> %at|%B", &merge]),
        "newgit <newgit@localhost> 6000|Import t3code into apps/t3code"
    );
    assert_eq!(
        git(repo, &["ls-tree", "-r", "--name-only", &merge]),
        "README.md\napps/t3code/app.js\napps/t3code/side.js"
    );

    // `git log` and `git blame` reach through the merge into the real history.
    let touched = git(repo, &["log", "--format=%H", &merge, "--", "apps/t3code"]);
    for imported in &project.imported {
        assert!(touched.contains(imported.as_str()), "{imported} in log");
    }
    let blame = git(
        repo,
        &["blame", "--porcelain", &merge, "--", "apps/t3code/app.js"],
    );
    assert!(blame.starts_with(&project.imported[0]));

    // Composed where the join is not already a commit, it comes out the same.
    git(repo, &["update-ref", "-d", "refs/newgit/views/main"]);
    let state = compose::read_state(repo, &merge).expect("state");
    assert_eq!(state.get("t3code"), Some(&joined.marker));
    assert_eq!(state.get("root"), Some(&project.b));
}

#[test]
fn work_after_the_import_follows_the_merge_and_keeps_its_ids() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    let (root, t3code) = definitions();
    let all = [root.clone(), t3code.clone()];
    let joined = import(&project, &t3code);
    let merge = view(
        repo,
        "main",
        &tips(&root, &project.b, &t3code, &joined.marker),
        &all,
    );

    // A commit on top of the merge touching both trackers, dated before
    // everything (a skewed clock): it still goes after the merge.
    git(repo, &["checkout", "-q", "--detach", &merge]);
    let work = commit(
        repo,
        &[
            ("README.md", "hello, t3code\n"),
            ("apps/t3code/app.js", "one\ntwo\nthree\n"),
        ],
        "use t3code",
        500,
    );
    let split = compose::split(repo, &merge, &work, &[&root, &t3code], &all).expect("split");
    assert_eq!(split.made.len(), 2, "one commit per tracker");
    let recomposed = view(
        repo,
        "main",
        &tips(&root, &split.state["root"], &t3code, &split.state["t3code"]),
        &all,
    );
    assert_eq!(recomposed, work, "split and recomposed, the same commit");
}

#[test]
fn a_branch_from_before_the_import_has_no_t3code_until_it_takes_it() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    let (root, t3code) = definitions();
    let all = [root.clone(), t3code.clone()];
    let joined = import(&project, &t3code);

    // Forked at `a`, with work of its own; t3code's `main` is all it can see.
    git(repo, &["checkout", "-q", "--detach", &project.a]);
    let c = commit(repo, &[("notes.txt", "mine\n")], "my work", 7_000);
    let before = view(repo, "old", &tips(&root, &c, &t3code, &joined.marker), &all);
    assert_eq!(
        before, c,
        "nothing is rewritten, and nothing of t3code is there"
    );

    // Taking it: one join on top of the branch, on t3code as `main` has it.
    let taken =
        compose::join(repo, &t3code, &c, &joined.marker, "refs/test/t3code/old").expect("take");
    let after = view(repo, "old", &tips(&root, &c, &t3code, &taken.marker), &all);
    assert_eq!(after, taken.view);
    assert_eq!(
        git(repo, &["rev-parse", &format!("{after}^1")]),
        c,
        "`c` keeps its id"
    );
    assert_eq!(
        git(repo, &["ls-tree", "-r", "--name-only", &after]),
        "README.md\napps/t3code/app.js\napps/t3code/side.js\nnotes.txt",
        "t3code, without root's `b`"
    );
    assert_eq!(git(repo, &["show", &format!("{after}:README.md")]), "hello");
}

#[test]
fn a_second_import_can_join_after_the_first() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    let (root, t3code) = definitions();
    let other = tracker("other", &["apps/other"], &[]);
    let root = TrackerDefinition {
        excluded: vec!["apps/t3code".into(), "apps/other".into()],
        ..root
    };
    let all = [root.clone(), t3code.clone(), other.clone()];
    let joined = import(&project, &t3code);

    git(repo, &["checkout", "-q", "--orphan", "other"]);
    git(repo, &["rm", "-rfq", "."]);
    let o = commit(repo, &[("apps/other/x", "x\n")], "other: start", 900);
    git(repo, &["checkout", "-q", "main"]);
    let second = compose::join(repo, &other, &joined.view, &o, "refs/test/other/main")
        .expect("join after the first join");

    let mut both = tips(&root, &project.b, &t3code, &joined.marker);
    both.push(TrackerTip {
        definition: &other,
        commit: second.marker.clone(),
    });
    let tip = view(repo, "main", &both, &all);
    assert_eq!(tip, second.view);
    assert_eq!(git(repo, &["rev-parse", &format!("{tip}^1")]), joined.view);
}

/// Not an assertion so much as a look: the whole flow, printing each view.
/// `cargo test -p newgit-core --test compose_import walkthrough -- --nocapture`
#[test]
fn walkthrough_import_take_work_merge_back() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    let (root, t3code) = definitions();
    let all = [root.clone(), t3code.clone()];
    let graph = |title: &str, tip: &str| {
        println!(
            "\n== {title}\n{}",
            git(
                repo,
                &[
                    "log",
                    "--graph",
                    "--format=%h %an: %s (%ad)",
                    "--date=unix",
                    tip
                ]
            )
        );
    };

    // 1. The import on `main`.
    let joined = import(&project, &t3code);
    let main = view(
        repo,
        "main",
        &tips(&root, &project.b, &t3code, &joined.marker),
        &all,
    );
    graph("main after the import", &main);

    // 2. An instance from before the import, with its own work.
    git(repo, &["checkout", "-q", "--detach", &project.a]);
    let c = commit(repo, &[("notes.txt", "mine\n")], "instance: my work", 7_000);
    graph(
        "the old instance",
        &view(repo, "old", &tips(&root, &c, &t3code, &joined.marker), &all),
    );

    // 3. It takes t3code, then works on both.
    let taken =
        compose::join(repo, &t3code, &c, &joined.marker, "refs/test/t3code/old").expect("take");
    let instance = view(repo, "old", &tips(&root, &c, &t3code, &taken.marker), &all);
    git(repo, &["checkout", "-q", "--detach", &instance]);
    let d = commit(
        repo,
        &[
            ("notes.txt", "mine, with t3code\n"),
            ("apps/t3code/app.js", "one\ntwo\nmine\n"),
        ],
        "instance: use t3code",
        8_000,
    );
    let split = compose::split(repo, &instance, &d, &[&root, &t3code], &all).expect("split");
    let instance = view(
        repo,
        "old",
        &tips(&root, &split.state["root"], &t3code, &split.state["t3code"]),
        &all,
    );
    assert_eq!(instance, d, "the instance's own commits keep their ids");
    graph(
        "the old instance after taking t3code and working",
        &instance,
    );

    // 4. Merging it into `main`, tracker by tracker: root's branches really
    // merge; t3code's `main` fast-forwards to the instance's.
    let root_tree = git(
        repo,
        &[
            "merge-tree",
            "--write-tree",
            &project.b,
            &split.state["root"],
        ],
    );
    let root_main = git_env(
        repo,
        &[
            "commit-tree",
            &root_tree,
            "-p",
            &project.b,
            "-p",
            &split.state["root"],
            "-m",
            "Merge instance old",
        ],
        &who("jack", 9_000),
    );
    let merged = view(
        repo,
        "main",
        &tips(&root, &root_main, &t3code, &split.state["t3code"]),
        &all,
    );
    graph("main after merging the instance", &merged);
    let first_parents = git(repo, &["rev-list", "--first-parent", &merged]);
    assert!(
        first_parents.contains(&main),
        "main's history before the merge is untouched"
    );
    let reachable = git(repo, &["rev-list", &merged]);
    println!(
        "\nthe instance's commits in main's view: c {}, join {}, d {}",
        if reachable.contains(&c) {
            "same id"
        } else {
            "new id"
        },
        if reachable.contains(&taken.view) {
            "same id"
        } else {
            "new id"
        },
        if reachable.contains(&d) {
            "same id"
        } else {
            "new id"
        },
    );
    println!(
        "main's files:\n{}",
        git(repo, &["ls-tree", "-r", "--name-only", &merged])
    );
}
