//! Composing keeps each tracker's own commit order whatever its dates say:
//! real histories are not in date order (rebases, cherry-picks, skewed
//! clocks), and a view of them must still be the same commits.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, TrackerTip};
use newgit_core::tracker::{Storage, TrackerDefinition};

const VIEW: &str = "refs/newgit/views/check";

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
        excluded: excluded.iter().map(Utf8PathBuf::from).collect(),
        definition_rev: "sha256:000000000000".to_owned(),
    }
}

/// A project whose history mixes app code and `apps/electron` — including a
/// commit that touches both.
#[test]
fn a_history_whose_dates_go_backwards_composes_into_itself() {
    let (_guard, temp) = tempdir();
    let repo = temp.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    // Dates out of order along the history: a later commit is older.
    commit(&repo, &[("a.txt", "1\n")], "first", "ana", 3_000);
    commit(
        &repo,
        &[("b.txt", "2\n")],
        "rebased, older date",
        "ana",
        1_000,
    );
    commit(
        &repo,
        &[("a.txt", "3\n"), ("apps/x/y", "1\n")],
        "third",
        "ana",
        2_000,
    );
    commit(&repo, &[("c.txt", "4\n")], "fourth", "ana", 500);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let root = tracker("root", &["."], &[], false);
    let view = compose::compose(
        &repo,
        VIEW,
        &[TrackerTip {
            definition: &root,
            commit: head.clone(),
        }],
        &[],
    )
    .expect("compose");
    assert_eq!(
        view.tip.as_deref(),
        Some(head.as_str()),
        "same commits, same ids"
    );

    // And carved into two trackers, it still folds back into itself.
    let apps = tracker("apps", &["apps/x"], &[], false);
    let carved = compose::carve(&repo, &head, &apps, "refs/newgit/trackers/apps/main")
        .expect("carve")
        .expect("apps has history");
    let root = tracker("root", &["."], &["apps/x"], false);
    let view = compose::compose(
        &repo,
        VIEW,
        &[
            TrackerTip {
                definition: &root,
                commit: head.clone(),
            },
            TrackerTip {
                definition: &apps,
                commit: carved,
            },
        ],
        &[],
    )
    .expect("compose");
    assert_eq!(view.tip.as_deref(), Some(head.as_str()));
}
