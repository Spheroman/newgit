//! `newgit tracker create <name> --git --mount <dir> --from <url>` (#140;
//! docs/design/trackers.md, *The t3code milestone*): an outside repository's
//! history becomes the tracker's `main` unchanged, and one commit in the
//! project root adds the definition, the mount, and the pin, composing as a
//! merge that brings that history in under the mount.

mod imports;

use std::process::Command;

use camino::Utf8Path;
use imports::*;

#[test]
fn an_import_keeps_every_id_and_comes_in_through_one_merge() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    std::fs::write(upstream.join(".gitmodules"), "[submodule \"x\"]\n").expect("write");
    let tip = commit_as(&upstream, "theo", 1_000_400, &[], "t3: submodules");
    let store = project(&temp);
    let repo = store.paths().project_root.clone();
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let outcome = import(&store, &upstream);
    assert_eq!(outcome.main, tip, "the tracker's `main` is upstream's tip");
    assert_eq!(outcome.commits, 5);
    assert!(outcome.submodules, "the .gitmodules is noted");
    assert_eq!(
        store_ref(&store, "refs/trackers/t3code/main").as_deref(),
        Some(tip.as_str())
    );
    // Every commit is upstream's own, byte for byte.
    let tracker_store = store.paths().tracker_store.clone();
    let ours = git(
        &repo,
        &[
            &format!("--git-dir={tracker_store}"),
            "rev-list",
            "refs/trackers/t3code/main",
        ],
    );
    assert_eq!(ours, git(&upstream, &["rev-list", "newgit"]));

    // The project root took one commit: a merge bringing it in.
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), outcome.view);
    assert_eq!(git(&repo, &["rev-parse", "HEAD^1"]), before);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Import `t3code` at apps/t3code from upstream",
        "a local path is recorded as its directory's name, never the path"
    );
    assert_eq!(
        git(&repo, &["log", "--format=%s", "HEAD^1..HEAD^2"]),
        "t3: submodules\nMerge side\nt3: second\nt3: side\nt3: first",
        "upstream's history, merge and all, replayed under the mount"
    );
    assert_eq!(
        git(
            &repo,
            &["ls-tree", "-r", "--name-only", "HEAD", "apps/t3code"]
        ),
        "apps/t3code/.gitmodules\napps/t3code/package.json\napps/t3code/src/app.ts\napps/t3code/src/side.ts"
    );
    assert!(repo.join(".newgit/trackers/t3code.toml").is_file());
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(
        git(
            &repo,
            &["log", "--format=%s", "--", "apps/t3code/src/side.ts"]
        ),
        "t3: side",
        "`git log` reaches through the merge"
    );
    let blame = git(&repo, &["blame", "--porcelain", "apps/t3code/src/app.ts"]);
    assert!(blame.contains("author theo"), "{blame}");
    assert!(blame.contains("summary t3: second"), "{blame}");

    // Root pins it, and the definition is `access`'s.
    let pinned = git(
        &repo,
        &[
            &format!("--git-dir={tracker_store}"),
            "rev-parse",
            "refs/trackers/root/instances/main:.newgit/pins/t3code",
        ],
    );
    assert_eq!(pinned, tip);
    let access = git(
        &repo,
        &[
            &format!("--git-dir={tracker_store}"),
            "ls-tree",
            "--name-only",
            "refs/trackers/access/instances/main",
        ],
    );
    assert!(access.lines().any(|line| line == "t3code.toml"), "{access}");

    // It publishes like any new tracker, and a spawn has it.
    manager(&store).merge("main", "main").expect("merge");
    let spawned = manager(&store).spawn("after", None).expect("spawn");
    assert!(
        spawned
            .branch
            .workspace_path
            .join("apps/t3code/src/side.ts")
            .is_file()
    );
    assert_eq!(
        git(&spawned.branch.workspace_path, &["rev-parse", "HEAD"]),
        outcome.view
    );
}

#[test]
fn a_signed_upstream_commit_comes_in_signed_and_verifies() {
    let (_guard, temp) = tempdir();
    let key = temp.join("signing-key");
    let made = Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "theo@example.com",
            "-f",
            key.as_str(),
        ])
        .status();
    if !made.is_ok_and(|status| status.success()) {
        eprintln!("ssh-keygen unavailable; skipping");
        return;
    }
    let upstream = upstream(&temp);
    for (name, value) in [
        ("gpg.format", "ssh"),
        ("user.signingkey", key.as_str()),
        ("commit.gpgSign", "true"),
    ] {
        git(&upstream, &["config", name, value]);
    }
    let signed = commit_as(
        &upstream,
        "theo",
        1_000_500,
        &[("signed.ts", "s\n")],
        "t3: signed",
    );
    let raw = git(&upstream, &["cat-file", "commit", &signed]);
    assert!(raw.contains("gpgsig"), "{raw}");
    let store = project(&temp);
    import(&store, &upstream);

    let tracker_store = store.paths().tracker_store.clone();
    let dir = format!("--git-dir={tracker_store}");
    assert_eq!(
        git(
            &temp,
            &[&dir, "cat-file", "commit", "refs/trackers/t3code/main"]
        ),
        raw
    );
    let public = std::fs::read_to_string(format!("{key}.pub")).expect("public key");
    let allowed = temp.join("allowed");
    std::fs::write(&allowed, format!("theo@example.com {public}")).expect("write");
    git(
        &temp,
        &[
            &dir,
            "-c",
            "gpg.format=ssh",
            "-c",
            &format!("gpg.ssh.allowedSignersFile={allowed}"),
            "verify-commit",
            "refs/trackers/t3code/main",
        ],
    );
}

/// Nothing an import refused leaves anything behind: no refs, no
/// definition, no commit, no files.
fn assert_nothing_left(store: &newgit_core::store::MetadataStore, head: &str) {
    let repo = store.paths().project_root.clone();
    assert_eq!(refs_mentioning(store, "t3code"), Vec::<String>::new());
    assert_eq!(refs_mentioning(store, "importing"), Vec::<String>::new());
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert!(!repo.join(".newgit/trackers/t3code.toml").exists());
}

#[test]
fn an_import_into_a_mount_with_committed_files_is_refused() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    let repo = store.paths().project_root.clone();
    commit_as(
        &repo,
        "ana",
        1_500_000_000,
        &[("apps/t3code/old.ts", "old\n")],
        "old t3code",
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let error = manager(&store)
        .import_git_tracker(
            "t3code",
            "project-devs",
            Utf8Path::new("apps/t3code"),
            upstream.as_str(),
            Some("newgit"),
        )
        .expect_err("refused");
    assert!(error.to_string().contains("already has files"), "{error}");
    assert!(error.to_string().contains("--path apps/t3code"), "{error}");
    assert_nothing_left(&store, &head);
}

#[test]
fn an_import_into_a_mount_with_files_on_disk_is_refused() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::create_dir_all(repo.join("apps/t3code")).expect("mkdir");
    std::fs::write(repo.join("apps/t3code/notes.md"), "mine\n").expect("write");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let error = manager(&store)
        .import_git_tracker(
            "t3code",
            "project-devs",
            Utf8Path::new("apps/t3code"),
            upstream.as_str(),
            Some("newgit"),
        )
        .expect_err("refused");
    assert!(error.to_string().contains("already has files"), "{error}");
    assert_nothing_left(&store, &head);
    assert_eq!(
        std::fs::read_to_string(repo.join("apps/t3code/notes.md")).expect("kept"),
        "mine\n"
    );
}

#[test]
fn an_import_adding_a_file_a_tracker_outside_git_owns_is_refused() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    commit_as(
        &upstream,
        "theo",
        1_000_600,
        &[(".env", "SECRET=1\n")],
        "t3: oops",
    );
    let store = project(&temp);
    let repo = store.paths().project_root.clone();
    // `t3-env` owns the `.env` under the mount, outside Git.
    std::fs::write(
        repo.join(".newgit/trackers/t3-env.toml"),
        "audience = \"user\"\nstorage = \"local\"\nmount = \"apps/t3code\"\npaths = [\".env\"]\n",
    )
    .expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "t3-env"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let error = manager(&store)
        .import_git_tracker(
            "t3code",
            "project-devs",
            Utf8Path::new("apps/t3code"),
            upstream.as_str(),
            Some("newgit"),
        )
        .expect_err("refused");
    let text = error.to_string();
    assert!(
        text.contains("`apps/t3code/.env`") && text.contains("`t3-env`"),
        "{text}"
    );
    assert_nothing_left(&store, &head);
}

#[test]
fn an_old_branch_has_no_import_until_it_merges_main() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    let repo = store.paths().project_root.clone();
    let old = manager(&store)
        .spawn("old", None)
        .expect("spawn")
        .branch
        .workspace_path;
    let outcome = import(&store, &upstream);
    manager(&store).merge("main", "main").expect("publish");

    // The old instance's work goes on without it.
    commit_as(
        &old,
        "ana",
        1_600_000_000,
        &[("old.txt", "x\n")],
        "old work",
    );
    manager(&store).checkpoint("old", None).expect("checkpoint");
    assert!(!old.join("apps/t3code").exists());
    let tracker_store = store.paths().tracker_store.clone();
    let dir = format!("--git-dir={tracker_store}");
    assert!(!git_ok(
        &temp,
        &[
            &dir,
            "rev-parse",
            "-q",
            "--verify",
            "refs/trackers/root/instances/old:.newgit/pins/t3code"
        ]
    ));

    // Merging `main` brings it, with the very pin `main` has.
    git(
        &old,
        &[
            "pull",
            "-q",
            "--no-rebase",
            "--no-edit",
            repo.as_str(),
            "main",
        ],
    );
    assert!(old.join("apps/t3code/src/side.ts").is_file());
    assert!(git_ok(
        &old,
        &["merge-base", "--is-ancestor", &outcome.view, "HEAD"]
    ));
    manager(&store).merge("old", "main").expect("merge");
    let pinned = git(
        &temp,
        &[
            &dir,
            "rev-parse",
            "refs/trackers/root/main:.newgit/pins/t3code",
        ],
    );
    assert_eq!(pinned, outcome.main, "the merge made nothing in t3code");
    assert_eq!(
        store_ref(&store, "refs/trackers/t3code/main").as_deref(),
        Some(outcome.main.as_str())
    );
}
