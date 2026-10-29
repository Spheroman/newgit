//! Repairing tracker commits split before their trailers kept the
//! original's message ending and headers (docs/design/trackers.md, step 3e):
//! rewritten from the originals here, they recompose into the originals
//! anywhere, and repairing again changes nothing.

use std::process::{Command, Stdio};

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
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

/// `commit` as newgit split it before this fix: the same tree, parents, and
/// identity, its message trimmed of its final newlines and its headers
/// dropped, the trailers on, and one newline at the end.
fn as_split_before(repo: &Utf8Path, commit: &str, source: &str, parent: &str) -> String {
    let raw = git(repo, &["cat-file", "commit", commit]);
    let (_, message) = raw.split_once("\n\n").expect("message");
    let body = message
        .split("\n\nNewgit-Split-Header: ")
        .next()
        .expect("body");
    let body = body
        .split("\n\nNewgit-Split-Parent: ")
        .next()
        .expect("body");
    let old = format!("{body}\n\nNewgit-Split-Parent: {parent}\nNewgit-Split: {source}\n");
    let tree = git(repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let mut args = vec!["commit-tree", "--no-gpg-sign", tree.as_str(), "-F", "-"];
    let parents = git(repo, &["rev-parse", &format!("{commit}^@")]);
    for parent in parents.lines() {
        args.extend(["-p", parent]);
    }
    // `Name <email> <unix> <tz>`, as stored.
    let identity = |role: &str| {
        let value = raw
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{role} ")))
            .expect("identity");
        let (name, rest) = value.split_once(" <").expect("name");
        let (email, date) = rest.split_once("> ").expect("email");
        (name.to_owned(), email.to_owned(), format!("@{date}"))
    };
    let (author, author_email, author_date) = identity("author");
    let (committer, committer_email, committer_date) = identity("committer");
    let env = [
        ("GIT_AUTHOR_NAME", author),
        ("GIT_AUTHOR_EMAIL", author_email),
        ("GIT_AUTHOR_DATE", author_date),
        ("GIT_COMMITTER_NAME", committer),
        ("GIT_COMMITTER_EMAIL", committer_email),
        ("GIT_COMMITTER_DATE", committer_date),
    ];
    git_env(repo, &args, &env, Some(&old))
}

#[test]
fn repaired_splits_recompose_into_the_originals_and_repairing_again_does_nothing() {
    let guard = tempfile::tempdir().expect("tempdir");
    let temp = Utf8PathBuf::from_path_buf(guard.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    let repo = temp.join("project");
    std::fs::create_dir_all(repo.join("apps/electron")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
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

    // A GitHub-style merge on top: signed, no final newline.
    git(&repo, &["reset", "-q", "--hard", &base]);
    std::fs::write(repo.join("README.md"), "docs\n").expect("write");
    std::fs::write(repo.join("apps/electron/main.js"), "v2\n").expect("write");
    git(&repo, &["add", "-A"]);
    let tree = git(&repo, &["write-tree"]);
    let object = format!(
        "tree {tree}\nparent {base}\nauthor DJ <dj@example.com> 2000 -0700\n\
         committer GitHub <noreply@github.com> 2000 -0700\n\
         gpgsig -----BEGIN PGP SIGNATURE-----\n \n wsFcBAABCAAQBQJq39A0\n -----END PGP SIGNATURE-----\n \n\n\
         Both at once (#1)"
    );
    let merge = git_env(
        &repo,
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        &[],
        Some(&object),
    );
    let all = [root.clone(), electron.clone()];
    let outcome = compose::split(&repo, &base, &merge, &[&root, &electron], &all).expect("split");
    let (root_new, electron_new) = (
        outcome.state["root"].clone(),
        outcome.state["electron"].clone(),
    );

    // The same pieces, as they were split before the fix; the state notes
    // name them.
    let root_old = as_split_before(&repo, &root_new, &merge, &base);
    let electron_old = as_split_before(&repo, &electron_new, &merge, &base);
    git(
        &repo,
        &[
            "notes",
            "--ref",
            compose::NOTES_REF,
            "add",
            "-f",
            "-m",
            &format!("electron {electron_old}\nroot {root_old}"),
            &merge,
        ],
    );
    // And a teammate's commit after them, whose source is not here.
    let teammate = {
        let tree = git(&repo, &["rev-parse", &format!("{root_old}^{{tree}}")]);
        git_env(
            &repo,
            &[
                "commit-tree",
                "--no-gpg-sign",
                &tree,
                "-p",
                &root_old,
                "-F",
                "-",
            ],
            &who("bo", 3_000),
            Some(&format!("Theirs\n\nNewgit-Split: {}\n", "0".repeat(40))),
        )
    };

    let remote = temp.join("remote");
    std::fs::create_dir_all(&remote).expect("mkdir");
    git(&remote, &["init", "-q", "--bare"]);
    let compose_on_remote = |root_commit: &str, electron_commit: &str| {
        git(&repo, &["update-ref", "refs/split/root", root_commit]);
        git(
            &repo,
            &["update-ref", "refs/split/electron", electron_commit],
        );
        git(
            &remote,
            &[
                "fetch",
                "-q",
                "--force",
                repo.as_str(),
                "+refs/split/*:refs/split/*",
            ],
        );
        compose::compose(&remote, VIEW, &tips(root_commit, electron_commit), &[])
            .expect("compose on the remote")
            .tip
            .expect("tip")
    };
    assert_ne!(
        compose_on_remote(&root_old, &electron_old),
        merge,
        "split the old way, the merge comes back as a new commit"
    );

    let moved = compose::repair_splits(&repo, &[teammate.clone(), electron_old.clone()])
        .expect("repair")
        .moved;
    assert_eq!(
        moved.get(&root_old),
        Some(&root_new),
        "rebuilt exactly as split now"
    );
    assert_eq!(moved.get(&electron_old), Some(&electron_new));
    let teammate_new = moved.get(&teammate).expect("the teammate's commit moves");
    assert_eq!(
        git(&repo, &["rev-parse", &format!("{teammate_new}^")]),
        root_new,
        "onto the repaired commit"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%B", teammate_new]),
        git(&repo, &["log", "-1", "--format=%B", &teammate]),
        "with its message as it was: its source is not here to rewrite it from"
    );
    assert_eq!(
        compose_on_remote(&root_new, &electron_new),
        merge,
        "repaired, the merge comes back as itself"
    );

    assert_eq!(
        compose::remap_state_notes(&repo, &moved).expect("remap notes"),
        1
    );
    let state = compose::read_state(&repo, &merge).expect("state");
    assert_eq!(state["root"], root_new);
    assert_eq!(state["electron"], electron_new);

    let again = compose::repair_splits(&repo, &[teammate_new.clone(), electron_new.clone()])
        .expect("repair again");
    assert!(again.moved.is_empty(), "nothing left to repair: {again:?}");
}

#[test]
fn a_carved_commit_follows_the_tracker_commit_it_was_carved_from() {
    let guard = tempfile::tempdir().expect("tempdir");
    let repo = Utf8PathBuf::from_path_buf(guard.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    let empty = git(&repo, &["mktree"]);
    let commit = |parents: &[&str], message: &str, time: i64| {
        let mut args = vec!["commit-tree", "--no-gpg-sign", empty.as_str(), "-F", "-"];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        git_env(&repo, &args, &who("ana", time), Some(message))
    };
    let base = commit(&[], "initial\n", 1_000);
    // A view commit with no final newline, and root's piece of it, split
    // the old way.
    let view = commit(&[&base], "Fix (#1)", 2_000);
    let split = commit(
        &[&base],
        &format!("Fix (#1)\n\nNewgit-Split-Parent: {base}\nNewgit-Split: {view}\n"),
        2_000,
    );
    // Root's own merge on top — no trailers, so it moves only because its
    // parent does — and a tracker carved out of root afterwards, whose
    // commit names that merge.
    let merge = commit(&[&split], "Merge branch 'main'\n", 3_000);
    let carved = commit(
        &[],
        &format!("Merge branch 'main'\n\nNewgit-Split-Parent: {split}\nNewgit-Split: {merge}\n"),
        3_000,
    );

    let repair = compose::repair_splits(&repo, &[merge.clone(), carved.clone()]).expect("repair");
    let moved = &repair.moved;
    let (split_new, merge_new, carved_new) = (
        moved.get(&split).expect("the split moves"),
        moved.get(&merge).expect("the merge moves onto it"),
        moved.get(&carved).expect("the carved commit follows"),
    );
    assert_eq!(moved.len(), 3, "{moved:?}");
    assert_eq!(
        git(&repo, &["rev-parse", &format!("{merge_new}^")]),
        *split_new
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%B", carved_new]),
        format!(
            "Merge branch 'main'\n\nNewgit-Split-Parent: {split_new}\nNewgit-Split: {merge_new}"
        ),
        "its trailers name the commits as they are now"
    );
    let again = compose::repair_splits(&repo, &[merge_new.clone(), carved_new.clone()])
        .expect("repair again");
    assert!(again.moved.is_empty(), "{again:?}");
}
