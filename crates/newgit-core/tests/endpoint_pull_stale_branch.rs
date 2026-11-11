//! A pull request merged on a mirror from a branch made before `main` moved a
//! tracker the mirror withholds. The merge's parents disagree about that
//! tracker, but only `main`'s side changed it since they forked, so it lands
//! with `main`'s pin, as Git's own three-way merge would say. (It was refused
//! as "merges two lines of `t3code`" while newgit.dev's webhook queue waited
//! behind it, for a Dependabot pull request on 2026-11-10.)

mod imports;
mod mirror_remote;

use imports::*;
use mirror_remote::*;
use newgit_core::manager::RemoteExportOptions;

#[test]
fn a_branch_from_before_a_withheld_tracker_moved_merges_with_mains_pin() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let before = remote.index();

    // A pull request branches off the mirror's `main` as it is now.
    let clone = temp.join("pr");
    git(
        &temp,
        &[
            "clone",
            "-q",
            "-b",
            "main",
            remote.mirror.as_str(),
            clone.as_str(),
        ],
    );
    git(&clone, &["config", "commit.gpgSign", "false"]);
    git(&clone, &["switch", "-q", "-c", "topic"]);
    commit_as(
        &clone,
        "bot",
        2_000_000_000,
        &[("deps.txt", "bumped\n")],
        "Bump deps",
    );
    git(&clone, &["push", "-q", "origin", "topic"]);

    // Then `main` moves `t3code`'s pin, and is exported.
    let upstream = temp.join("upstream");
    commit_as(
        &upstream,
        "theo",
        1_000_400,
        &[("src/app.ts", "three\n")],
        "t3: third",
    );
    let tracker_store = remote.store.paths().tracker_store.clone();
    git(
        &temp,
        &[
            &format!("--git-dir={tracker_store}"),
            "fetch",
            "-q",
            upstream.as_str(),
            "+refs/heads/newgit:refs/trackers/t3code/main",
        ],
    );
    manager(&remote.store)
        .pull(None, &["t3code".to_owned()])
        .expect("pull t3code");
    // ... alongside work the mirror does see, so its `main` moves too.
    let repo = remote.store.paths().project_root.clone();
    std::fs::write(repo.join("ours.txt"), "ours\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "ours"]);
    manager(&remote.store)
        .merge("main", "main")
        .expect("publish");
    manager(&remote.store)
        .export_to_remote("github", RemoteExportOptions::default())
        .expect("export");
    remote.publish();
    let base = remote.index();
    assert_ne!(
        base["t3code"], before["t3code"],
        "t3code's pin moved on main"
    );

    // The pull request is merged on the mirror.
    git(&clone, &["fetch", "-q", "origin", "main"]);
    git(&clone, &["switch", "-q", "-C", "main", "origin/main"]);
    git(
        &clone,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #2",
            "topic",
        ],
    );
    git(&clone, &["push", "-q", "origin", "main"]);
    let theirs = git(&clone, &["rev-parse", "HEAD"]);

    let list = remote.landed(&base, &theirs);
    assert_eq!(list["t3code"], base["t3code"], "main's t3code pin is kept");
    let root = &remote.repos["root"];
    assert_eq!(git(root, &["show", "main:deps.txt"]), "bumped");
    assert_eq!(
        git(root, &["rev-parse", "main:.newgit/pins/t3code"]),
        base["t3code"],
    );
}
