//! An imported tracker (newgit-trackers.md, *An imported tracker joins
//! through one merge*): a history from outside the project joins the view
//! through one merge on its anchor, rather than interleaving by date, so
//! adding it rewrites nothing.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, TrackerTip};
use newgit_core::tracker::{Imported, Storage, TrackerDefinition};

const VIEW: &str = "refs/newgit/views/test";

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
        imported: None,
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

fn definitions(project: &Project) -> (TrackerDefinition, TrackerDefinition) {
    let root = tracker("root", &["."], &["apps/t3code"]);
    let mut t3code = tracker("t3code", &["apps/t3code"], &[]);
    t3code.imported = Some(Imported {
        anchor: project.b.clone(),
        tip: project.imported.last().expect("tip").clone(),
    });
    (root, t3code)
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

#[test]
fn an_imported_tracker_joins_through_one_merge_on_its_anchor() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    let (root, t3code) = definitions(&project);
    let import_tip = project.imported.last().expect("tip");

    let view = compose::compose(
        repo,
        VIEW,
        &tips(&root, &project.b, &t3code, import_tip),
        &[root.clone(), t3code.clone()],
    )
    .expect("compose");
    let merge = view.tip.expect("a view");

    // One commit appended; everything before it is the original history.
    assert_eq!(git(repo, &["rev-parse", &format!("{merge}^1")]), project.b);
    assert_eq!(
        git(repo, &["rev-parse", &format!("{merge}^2")]),
        *import_tip
    );
    assert_eq!(
        git(repo, &["rev-list", "--first-parent", &merge]),
        format!("{merge}\n{}\n{}", project.b, project.a),
        "the project's commits keep their ids"
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%an <%ae> %at|%s", &merge]),
        "newgit <newgit@localhost> 6000|Import t3code into apps/t3code"
    );

    // The anchor's tree plus the tracker's paths at the tip.
    assert_eq!(
        git(repo, &["ls-tree", "-r", "--name-only", &merge]),
        "README.md\napps/t3code/app.js\napps/t3code/side.js"
    );
    assert_eq!(
        git(repo, &["show", &format!("{merge}:README.md")]),
        "hello, world"
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
    assert!(
        blame.starts_with(&project.imported[0]),
        "the first line is blamed on the imported commit that wrote it"
    );

    // Deterministic: another composer gets the same commit.
    let again = compose::compose(
        repo,
        "refs/newgit/views/again",
        &tips(&root, &project.b, &t3code, import_tip),
        &[root.clone(), t3code.clone()],
    )
    .expect("compose");
    assert_eq!(again.tip.as_deref(), Some(merge.as_str()));
    let state = compose::read_state(repo, &merge).expect("state");
    assert_eq!(state.get("t3code"), Some(import_tip));
    assert_eq!(state.get("root"), Some(&project.b));
}

#[test]
fn work_after_the_import_follows_the_merge_and_keeps_its_ids() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    let (root, t3code) = definitions(&project);
    let all = [root.clone(), t3code.clone()];
    let import_tip = project.imported.last().expect("tip");
    let merge = compose::compose(
        repo,
        VIEW,
        &tips(&root, &project.b, &t3code, import_tip),
        &all,
    )
    .expect("compose")
    .tip
    .expect("a view");

    // A commit made in the view on top of the merge, touching both trackers,
    // dated before everything (a skewed clock): it still goes after the merge.
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

    let recomposed = compose::compose(
        repo,
        VIEW,
        &tips(&root, &split.state["root"], &t3code, &split.state["t3code"]),
        &all,
    )
    .expect("compose");
    assert_eq!(
        recomposed.tip.as_deref(),
        Some(work.as_str()),
        "split and recomposed, the commit on top of the merge is the same commit"
    );
}

#[test]
fn a_view_without_the_anchor_leaves_the_imported_tracker_out() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    let (root, t3code) = definitions(&project);
    let import_tip = project.imported.last().expect("tip");

    // A branch forked before the import: root is still at `a`.
    let view = compose::compose(
        repo,
        VIEW,
        &tips(&root, &project.a, &t3code, import_tip),
        &[root.clone(), t3code.clone()],
    )
    .expect("compose");
    assert_eq!(
        view.tip.as_deref(),
        Some(project.a.as_str()),
        "nothing is rewritten, and nothing of t3code is there"
    );
    assert_eq!(view.commits, 1);
}

/// The project with one more root commit, `b`, that also writes
/// `apps/other` (plus `README.md` when `readme`), and `t3code` anchored on it.
fn anchored_on_other(
    project: &Project,
    readme: bool,
) -> (
    String,
    TrackerDefinition,
    TrackerDefinition,
    TrackerDefinition,
) {
    let repo = &project.repo;
    git(repo, &["checkout", "-q", "main"]);
    let mut files = vec![("apps/other/x", "x\n")];
    if readme {
        files.push(("README.md", "hello, other\n"));
    }
    let b = commit(repo, &files, "other", 6_500);
    let (root, mut t3code) = definitions(project);
    let root = TrackerDefinition {
        excluded: vec!["apps/t3code".into(), "apps/other".into()],
        ..root
    };
    t3code.imported.as_mut().expect("imported").anchor = b.clone();
    (b, root, t3code, tracker("other", &["apps/other"], &[]))
}

#[test]
fn a_rebuilt_anchor_still_gets_the_merge() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    // `b` also wrote `apps/other`, which this view does not bind, so `b` is
    // rebuilt without it and the merge is written on the rebuilt commit.
    let (b, root, t3code, other) = anchored_on_other(&project, true);
    let import_tip = project.imported.last().expect("tip");

    let view = compose::compose(
        repo,
        VIEW,
        &tips(&root, &b, &t3code, import_tip),
        &[root.clone(), t3code.clone(), other],
    )
    .expect("compose");
    let merge = view.tip.expect("a view");
    assert_ne!(
        git(repo, &["rev-parse", &format!("{merge}^1")]),
        b,
        "rebuilt"
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%s", &format!("{merge}^1")]),
        "other"
    );
    assert_eq!(
        git(repo, &["rev-parse", &format!("{merge}^2")]),
        *import_tip
    );
    assert_eq!(
        git(repo, &["ls-tree", "-r", "--name-only", &merge]),
        "README.md\napps/t3code/app.js\napps/t3code/side.js",
        "the anchor as this view has it, plus t3code"
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%an %at|%s", &merge]),
        "newgit 6500|Import t3code into apps/t3code"
    );
}

#[test]
fn a_dropped_anchor_joins_after_the_commit_before_it() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let repo = &project.repo;
    // `b` wrote only `apps/other`: a view without `other` has no commit for
    // it, so the merge goes where `b` would have been.
    let (b, root, t3code, other) = anchored_on_other(&project, false);
    let import_tip = project.imported.last().expect("tip");

    let view = compose::compose(
        repo,
        VIEW,
        &tips(&root, &b, &t3code, import_tip),
        &[root.clone(), t3code.clone(), other],
    )
    .expect("compose");
    let merge = view.tip.expect("a view");
    assert_eq!(
        git(repo, &["rev-parse", &format!("{merge}^1")]),
        project.b,
        "the commit before the dropped anchor, as itself"
    );
    assert_eq!(
        git(repo, &["rev-parse", &format!("{merge}^2")]),
        *import_tip
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%an %at|%s", &merge]),
        "newgit 6500|Import t3code into apps/t3code",
        "dated by the anchor, as in every other view"
    );
}
