//! The composed view (docs/design/trackers.md, step 3): replaying tracker
//! histories gives an ordinary history with real authors, dates, and
//! messages, and splitting then recomposing gives back the identical commits.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, TrackerTip};
use newgit_core::tracker::{Storage, TrackerDefinition};

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

fn commit(
    repo: &Utf8Path,
    files: &[(&str, &str)],
    message: &str,
    author: &str,
    time: i64,
) -> String {
    for (path, contents) in files {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
    }
    git(repo, &["add", "-A"]);
    git_env(
        repo,
        &["commit", "-q", "--no-gpg-sign", "-m", message],
        &who(author, time),
    );
    git(repo, &["rev-parse", "HEAD"])
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn tracker(name: &str, paths: &[&str], excluded: &[&str], secret: bool) -> TrackerDefinition {
    TrackerDefinition {
        name: name.to_owned(),
        audience: "project-devs".to_owned(),
        storage: Storage::Local,
        merge_with_source: true,
        paths: paths.iter().map(Utf8PathBuf::from).collect(),
        secret,
        maintainers: None,
        role: None,
        priority: 0,
        requires: Vec::new(),
        resources: Vec::new(),
        excluded: excluded.iter().map(Utf8PathBuf::from).collect(),
        definition_rev: "sha256:000000000000".to_owned(),
    }
}

/// A project whose history mixes app code and `apps/electron` — including a
/// commit that touches both.
fn project(temp: &Utf8Path) -> (Utf8PathBuf, Vec<String>) {
    let repo = temp.join("project");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    let commits = vec![
        commit(
            &repo,
            &[("README.md", "hello\n"), ("apps/electron/main.js", "v1\n")],
            "initial",
            "ana",
            1_000,
        ),
        commit(
            &repo,
            &[("README.md", "hello, world\n")],
            "readme",
            "jack",
            2_000,
        ),
        commit(
            &repo,
            &[("README.md", "docs\n"), ("apps/electron/main.js", "v2\n")],
            "both at once",
            "ana",
            3_000,
        ),
        commit(
            &repo,
            &[("apps/electron/main.js", "v3\n")],
            "electron only",
            "jack",
            4_000,
        ),
    ];
    (repo, commits)
}

/// Carve `apps/electron`'s history out of `commits`, as `tracker create`
/// will: each touching commit becomes an electron commit marked with its
/// source, carrying only electron's files.
fn carve_electron(repo: &Utf8Path, commits: &[String]) -> String {
    let mut parent: Option<String> = None;
    for original in commits {
        let touched = git(
            repo,
            &[
                "diff-tree",
                "--root",
                "-r",
                "--name-only",
                "--no-commit-id",
                original,
            ],
        );
        if !touched
            .lines()
            .any(|path| path.starts_with("apps/electron/"))
        {
            continue;
        }
        let index = repo.join(".git/carve-index");
        let env = [("GIT_INDEX_FILE", index.to_string())];
        git_env(repo, &["read-tree", "--empty"], &env);
        git_env(
            repo,
            &[
                "read-tree",
                "--prefix=apps/electron/",
                &format!("{original}:apps/electron"),
            ],
            &env,
        );
        let tree = git_env(repo, &["write-tree"], &env);
        let raw = git(repo, &["cat-file", "commit", original]);
        let message = raw.split_once("\n\n").expect("message").1;
        let author = raw
            .lines()
            .find_map(|line| line.strip_prefix("author "))
            .expect("author");
        let (name, rest) = author.split_once(" <").expect("name");
        let time: i64 = rest
            .split("> ")
            .nth(1)
            .and_then(|date| date.split(' ').next())
            .and_then(|t| t.parse().ok())
            .expect("time");
        let full = format!("{}\n\nNewgit-Split: {original}\n", message.trim_end());
        let mut args = vec!["commit-tree", tree.as_str(), "-m", full.as_str()];
        if let Some(parent) = &parent {
            args.extend(["-p", parent.as_str()]);
        }
        parent = Some(git_env(repo, &args, &who(name, time)));
    }
    parent.expect("electron has history")
}

#[test]
fn composing_root_alone_is_the_project_history_itself() {
    let (_guard, temp) = tempdir();
    let (repo, commits) = project(&temp);
    let root = TrackerDefinition::root();
    let view = compose::compose(
        &repo,
        VIEW,
        &[TrackerTip {
            definition: &root,
            commit: commits[3].clone(),
        }],
        &[],
    )
    .expect("compose");
    assert_eq!(view.commits, 4);
    assert_eq!(
        view.tip.as_deref(),
        Some(commits[3].as_str()),
        "same commits, same ids"
    );
}

#[test]
fn a_carved_tracker_composes_back_into_the_original_history() {
    let (_guard, temp) = tempdir();
    let (repo, commits) = project(&temp);
    let electron_tip = carve_electron(&repo, &commits);
    let root = tracker("root", &["."], &["apps/electron"], false);
    let electron = tracker("electron", &["apps/electron"], &[], false);

    let view = compose::compose(
        &repo,
        VIEW,
        &[
            TrackerTip {
                definition: &root,
                commit: commits[3].clone(),
            },
            TrackerTip {
                definition: &electron,
                commit: electron_tip.clone(),
            },
        ],
        &[],
    )
    .expect("compose");
    assert_eq!(
        view.tip.as_deref(),
        Some(commits[3].as_str()),
        "two trackers' histories replay into the original commits, the mixed one folded back"
    );
    let state = compose::read_state(&repo, &commits[3]).expect("state");
    assert_eq!(state.get("electron"), Some(&electron_tip));

    // Without electron bound, the view simply has no apps/electron.
    let alone = compose::compose(
        &repo,
        VIEW,
        &[TrackerTip {
            definition: &root,
            commit: commits[3].clone(),
        }],
        std::slice::from_ref(&electron),
    )
    .expect("compose");
    let files = git(
        &repo,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            alone.tip.as_deref().expect("tip"),
        ],
    );
    assert_eq!(files, "README.md");
    let log = git(
        &repo,
        &["log", "--format=%an %s", alone.tip.as_deref().expect("tip")],
    );
    assert_eq!(
        log, "ana both at once\njack readme\nana initial",
        "real authors and messages; electron-only commits are electron's"
    );
}

#[test]
fn splitting_an_agents_work_and_recomposing_gives_back_the_same_commits() {
    let (_guard, temp) = tempdir();
    let (repo, commits) = project(&temp);
    let electron_tip = carve_electron(&repo, &commits);
    let root = tracker("root", &["."], &["apps/electron"], false);
    let electron = tracker("electron", &["apps/electron"], &[], false);
    let tips = |root_commit: &str, electron_commit: &str| {
        vec![
            TrackerTip {
                definition: &root,
                commit: root_commit.to_owned(),
            },
            TrackerTip {
                definition: &electron,
                commit: electron_commit.to_owned(),
            },
        ]
    };
    let base = compose::compose(&repo, VIEW, &tips(&commits[3], &electron_tip), &[])
        .expect("compose")
        .tip
        .expect("tip");

    // An agent works on top of the view: one mixed commit, one electron-only.
    git(&repo, &["checkout", "-q", &base]);
    let first = commit(
        &repo,
        &[
            ("README.md", "agent docs\n"),
            ("apps/electron/main.js", "v4\n"),
        ],
        "agent: both",
        "agent",
        5_000,
    );
    let second = commit(
        &repo,
        &[("apps/electron/main.js", "v5\n")],
        "agent: electron",
        "agent",
        6_000,
    );

    let all = [root.clone(), electron.clone()];
    let outcome = compose::split(&repo, &base, &second, &[&root, &electron], &all).expect("split");
    assert_eq!(
        outcome
            .made
            .iter()
            .map(|(tracker, _)| tracker.as_str())
            .collect::<Vec<_>>(),
        ["electron", "root", "electron"],
        "the mixed commit became one commit per tracker"
    );
    let root_tip = outcome.state["root"].clone();
    let electron_split_tip = outcome.state["electron"].clone();
    assert_eq!(
        git(&repo, &["ls-tree", "-r", "--name-only", &root_tip]),
        "README.md",
        "root holds only root's files"
    );

    let recomposed = compose::compose(&repo, VIEW, &tips(&root_tip, &electron_split_tip), &[])
        .expect("recompose");
    assert_eq!(
        recomposed.tip.as_deref(),
        Some(second.as_str()),
        "the agent's commits, ids unchanged"
    );
    assert_eq!(git(&repo, &["rev-parse", &format!("{second}^")]), first);
}

#[test]
fn a_secret_file_in_a_commit_is_found_where_it_would_leave() {
    let (_guard, temp) = tempdir();
    let (repo, commits) = project(&temp);
    let root = tracker("root", &["."], &[".env.local"], false);
    let env = tracker("runtime-env", &[".env.local"], &[], true);
    let base = compose::compose(
        &repo,
        VIEW,
        &[TrackerTip {
            definition: &root,
            commit: commits[3].clone(),
        }],
        &[],
    )
    .expect("compose")
    .tip
    .expect("tip");
    git(&repo, &["checkout", "-q", &base]);
    std::fs::write(repo.join(".env.local"), "STRIPE_KEY=sk_live\n").expect("write");
    git(&repo, &["add", "-f", ".env.local"]);
    git_env(
        &repo,
        &["commit", "-q", "--no-gpg-sign", "-m", "oops"],
        &who("agent", 5_000),
    );
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    // Splitting is ownership only; the secret check runs over what leaves.
    let found = compose::find_secret(
        &repo,
        &[&tip, "--not", &base],
        &[],
        &[root.clone(), env.clone()],
    )
    .expect("scan");
    assert_eq!(
        found,
        Some((tip, ".env.local".to_owned(), "runtime-env".to_owned()))
    );
}

#[test]
fn a_path_owned_by_an_unbound_tracker_is_refused_with_the_fix() {
    let (_guard, temp) = tempdir();
    let (repo, commits) = project(&temp);
    let root = tracker("root", &["."], &["apps/electron"], false);
    let electron = tracker("electron", &["apps/electron"], &[], false);
    let base = compose::compose(
        &repo,
        VIEW,
        &[TrackerTip {
            definition: &root,
            commit: commits[3].clone(),
        }],
        &[],
    )
    .expect("compose")
    .tip
    .expect("tip");
    git(&repo, &["checkout", "-q", &base]);
    let tip = commit(
        &repo,
        &[("apps/electron/new.js", "x\n")],
        "agent: electron without binding it",
        "agent",
        5_000,
    );
    let error = compose::split(
        &repo,
        &base,
        &tip,
        &[&root],
        &[root.clone(), electron.clone()],
    )
    .expect_err("refused");
    assert!(
        error.to_string().contains("newgit tracker use electron"),
        "{error}"
    );
}

#[test]
fn merge_commits_survive_composition_whole() {
    let (_guard, temp) = tempdir();
    let (repo, _) = project(&temp);
    // A merged pull request: a side branch touching both root and electron,
    // merged with a real merge commit.
    git(&repo, &["checkout", "-q", "-b", "feature"]);
    commit(
        &repo,
        &[
            ("README.md", "feature docs\n"),
            ("apps/electron/main.js", "v-feature\n"),
        ],
        "feature work",
        "ana",
        5_000,
    );
    git(&repo, &["checkout", "-q", "main"]);
    commit(
        &repo,
        &[("NOTES.md", "main moved\n")],
        "main moves",
        "jack",
        6_000,
    );
    git_env(
        &repo,
        &[
            "merge",
            "-q",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            "Merge feature",
            "feature",
        ],
        &who("jack", 7_000),
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    // The real carve: it diffs every commit, merges included, against its
    // first parent.
    let root = tracker("root", &["."], &["apps/electron"], false);
    let electron = tracker("electron", &["apps/electron"], &[], false);
    let electron_tip = compose::carve(
        &repo,
        &head,
        &electron,
        "refs/newgit/trackers/electron/main",
    )
    .expect("carve")
    .expect("electron has history");
    let view = compose::compose(
        &repo,
        VIEW,
        &[
            TrackerTip {
                definition: &root,
                commit: head.clone(),
            },
            TrackerTip {
                definition: &electron,
                commit: electron_tip,
            },
        ],
        &[],
    )
    .expect("compose");
    assert_eq!(
        view.tip.as_deref(),
        Some(head.as_str()),
        "the original merge commit itself, both parents intact"
    );
    assert_eq!(
        git(&repo, &["rev-list", "--parents", "-n1", VIEW])
            .split(' ')
            .count(),
        3
    );
}

#[test]
fn a_merge_after_trackers_diverge_keeps_every_parent() {
    let (_guard, temp) = tempdir();
    let (repo, commits) = project(&temp);
    let root = tracker("root", &["."], &["apps/electron"], false);
    let electron = tracker("electron", &["apps/electron"], &[], false);
    let carved = compose::compose(
        &repo,
        VIEW,
        &[TrackerTip {
            definition: &root,
            commit: commits[3].clone(),
        }],
        &[],
    )
    .expect("compose");
    assert!(carved.tip.is_some());
    let electron_tip = compose::carve(
        &repo,
        &commits[3],
        &electron,
        "refs/newgit/trackers/electron/main",
    )
    .expect("carve")
    .expect("history");

    // Divergence: electron gains a commit root never had.
    git(&repo, &["checkout", "-q", &electron_tip]);
    let electron_only = commit(
        &repo,
        &[("apps/electron/main.js", "electron's own\n")],
        "electron: own work",
        "ana",
        4_500,
    );

    // Then root merges a feature branch.
    git(&repo, &["checkout", "-q", "-B", "main", &commits[3]]);
    git(&repo, &["checkout", "-q", "-b", "feature"]);
    commit(
        &repo,
        &[("FEATURE.md", "feature\n")],
        "feature work",
        "ana",
        5_000,
    );
    git(&repo, &["checkout", "-q", "main"]);
    commit(
        &repo,
        &[("NOTES.md", "main moved\n")],
        "main moves",
        "jack",
        6_000,
    );
    git_env(
        &repo,
        &[
            "merge",
            "-q",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            "Merge feature",
            "feature",
        ],
        &who("jack", 7_000),
    );
    let root_tip = git(&repo, &["rev-parse", "HEAD"]);

    let view = compose::compose(
        &repo,
        VIEW,
        &[
            TrackerTip {
                definition: &root,
                commit: root_tip.clone(),
            },
            TrackerTip {
                definition: &electron,
                commit: electron_only,
            },
        ],
        &[],
    )
    .expect("compose");
    let tip = view.tip.expect("tip");
    assert_ne!(
        tip, root_tip,
        "diverged histories cannot reuse the original merge"
    );
    let parents = git(&repo, &["rev-list", "--parents", "-n1", &tip]);
    assert_eq!(
        parents.split(' ').count(),
        3,
        "the rebuilt merge keeps both parents: {parents}"
    );
    let log = git(&repo, &["log", "--format=%s", &tip]);
    assert!(
        log.contains("feature work"),
        "the merged branch is in history:\n{log}"
    );
    assert!(log.contains("electron: own work"), "{log}");
    assert_eq!(
        git(&repo, &["show", &format!("{tip}:apps/electron/main.js")]),
        "electron's own"
    );
    assert_eq!(
        git(&repo, &["show", &format!("{tip}:FEATURE.md")]),
        "feature"
    );
}
