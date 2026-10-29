//! A split commit composed somewhere that does not have the original — the
//! remote — comes back as the original, byte for byte, when the original
//! was unsigned: its message ends exactly as it did, final newline or not.
//! GitHub's merge commits have no final newline, and adding one changed
//! every id from the first of them on.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, TrackerTip};
use newgit_core::tracker::{Storage, TrackerDefinition};

const VIEW: &str = "refs/newgit/views/test";

fn git_env(dir: &Utf8Path, args: &[&str], env: &[(&str, String)], input: Option<&str>) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    if let Some(input) = input {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    let output = child.wait_with_output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    git_env(dir, args, &[], None)
}

/// A commit object's exact bytes — no trimming, so a final newline counts.
fn raw_commit(dir: &Utf8Path, commit: &str) -> Vec<u8> {
    let output = Command::new("git")
        .args(["-C", dir.as_str(), "cat-file", "commit", commit])
        .output()
        .expect("git runs");
    assert!(output.status.success(), "{commit} is a commit");
    output.stdout
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

/// Write `files`, then commit them on top of `parent` with `message`
/// exactly as given — `git commit` would add a final newline.
fn commit_exact(
    repo: &Utf8Path,
    parent: &str,
    files: &[(&str, &str)],
    message: &str,
    time: i64,
) -> String {
    for (path, contents) in files {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
    }
    git(repo, &["add", "-A"]);
    let tree = git(repo, &["write-tree"]);
    let commit = git_env(
        repo,
        &[
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-p",
            parent,
            "-F",
            "-",
        ],
        &who("agent", time),
        Some(message),
    );
    git(repo, &["reset", "-q", "--soft", &commit]);
    commit
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

#[test]
fn a_split_commit_recomposes_elsewhere_with_its_message_and_id_intact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let temp = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    let repo = temp.join("project");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::create_dir_all(repo.join("apps/electron")).expect("mkdir");
    std::fs::write(repo.join("apps/electron/main.js"), "v1\n").expect("write");
    git(&repo, &["add", "-A"]);
    git_env(
        &repo,
        &["commit", "-q", "--no-gpg-sign", "-m", "initial"],
        &who("ana", 1_000),
        None,
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let root = tracker("root", &["."], &["apps/electron"]);
    let electron = tracker("electron", &["apps/electron"], &[]);
    let electron_tip = compose::carve(
        &repo,
        &head,
        &electron,
        "refs/newgit/trackers/electron/main",
    )
    .expect("carve")
    .expect("electron has history");
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
    let base = compose::compose(&repo, VIEW, &tips(&head, &electron_tip), &[])
        .expect("compose")
        .tip
        .expect("tip");

    // A mixed commit whose message has no final newline, as GitHub writes a
    // merge's, then one ending in a blank line.
    git(&repo, &["reset", "-q", "--hard", &base]);
    let first = commit_exact(
        &repo,
        &base,
        &[("README.md", "docs\n"), ("apps/electron/main.js", "v2\n")],
        "Both at once (#1)\n\nCo-authored-by: Bo <bo@example.com>",
        2_000,
    );
    let second = commit_exact(
        &repo,
        &first,
        &[("README.md", "more docs\n")],
        "Root only\n\n",
        3_000,
    );

    let all = [root.clone(), electron.clone()];
    let outcome = compose::split(&repo, &base, &second, &[&root, &electron], &all).expect("split");
    git(
        &repo,
        &["update-ref", "refs/split/root", &outcome.state["root"]],
    );
    git(
        &repo,
        &[
            "update-ref",
            "refs/split/electron",
            &outcome.state["electron"],
        ],
    );

    // Somewhere with only the tracker commits, as the remote has.
    let remote = temp.join("remote");
    std::fs::create_dir_all(&remote).expect("mkdir");
    git(&remote, &["init", "-q", "--bare"]);
    git(
        &remote,
        &["fetch", "-q", repo.as_str(), "refs/split/*:refs/split/*"],
    );
    for original in [&first, &second] {
        let present = Command::new("git")
            .args(["-C", remote.as_str(), "cat-file", "-e", original])
            .status()
            .expect("git runs");
        assert!(!present.success(), "the remote never has the view commits");
    }

    let recomposed = compose::compose(
        &remote,
        VIEW,
        &tips(&outcome.state["root"], &outcome.state["electron"]),
        &[],
    )
    .expect("recompose");
    let rebuilt = recomposed.tip.expect("tip");
    assert_eq!(
        String::from_utf8_lossy(&raw_commit(&remote, &format!("{rebuilt}^"))),
        String::from_utf8_lossy(&raw_commit(&repo, &first)),
        "the rebuilt commit is the original, byte for byte"
    );
    assert_eq!(rebuilt, second, "so the ids are the originals'");
}
