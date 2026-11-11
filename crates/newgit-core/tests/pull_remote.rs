//! `newgit pull --remote <target>` (#142): a target's new commits come back
//! into its trackers. Rooted at a tracker, they are its own history: its
//! `main` takes them and its pin moves, one commit on top, whatever their
//! dates. Otherwise they are view commits:
//! ours are recognised, and the rest come in as work.

mod imports;

use imports::*;
use newgit_core::manager::RemoteExportOptions;
use newgit_core::store::MetadataStore;

/// An imported t3code, exported rooted to a copy of its upstream.
fn exported(temp: &camino::Utf8Path) -> (MetadataStore, camino::Utf8PathBuf, String) {
    let upstream = upstream(temp);
    let store = project(temp);
    let github = github_copy(temp, &upstream, "t3code");
    let outcome = import(&store, &upstream);
    set_export(
        &store,
        "t3code",
        Some(&export_to(&github, "t3code", "branch = \"newgit\"")),
    );
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export target"]);
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    (store, github, outcome.main)
}

#[test]
fn a_pr_merged_upstream_comes_back_with_pull_remote_and_newgit_pull() {
    let (_guard, temp) = tempdir();
    let (store, github, _) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let feature = manager(&store)
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    let theirs = merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: from a PR",
    );
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let pulled = manager(&store).pull_remote("t3code").expect("pull");
    assert_eq!(pulled.commits, 2);
    assert!(!pulled.merged, "t3code's `main` fast-forwarded");
    assert_eq!(
        store_ref(&store, "refs/trackers/t3code/main").as_deref(),
        Some(theirs.as_str())
    );
    let view = pulled.view.expect("a view commit");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), view);
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD^@"]),
        before,
        "one commit on one parent"
    );
    assert!(git(&repo, &["log", "-1", "--format=%s"]).starts_with("Pull t3code: 2 commits from "));
    assert!(repo.join("apps/t3code/src/pr.ts").is_file());
    // Again: nothing new.
    assert!(
        manager(&store)
            .pull_remote("t3code")
            .expect("again")
            .view
            .is_none()
    );

    // Published, an instance brings it in with `newgit pull t3code`, and the
    // next export is a no-op.
    manager(&store).merge("main", "main").expect("merge");
    let caught = manager(&store)
        .pull(Some("feature"), &["t3code".to_owned()])
        .expect("pull t3code");
    assert_eq!(caught.pulled[0].commits, 2);
    assert!(feature.join("apps/t3code/src/pr.ts").is_file());
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    assert_eq!(git(&github, &["rev-parse", "newgit"]), theirs);
}

#[test]
fn an_upstream_sync_with_old_dates_lands_on_top() {
    let (_guard, temp) = tempdir();
    let (store, github, main) = exported(&temp);
    let repo = store.paths().project_root.clone();
    // pingdotgg/t3code merged into the fork: commits dated long before the
    // import, off its first commit.
    let sync = temp.join("sync");
    git(
        &temp,
        &[
            "clone",
            "-q",
            "-b",
            "newgit",
            github.as_str(),
            sync.as_str(),
        ],
    );
    git(&sync, &["config", "commit.gpgSign", "false"]);
    let first = git(&sync, &["rev-list", "--max-parents=0", "HEAD"]);
    git(&sync, &["switch", "-q", "-c", "upstream", &first]);
    let old = commit_as(
        &sync,
        "theo",
        900_000,
        &[("src/old.ts", "old\n")],
        "t3: old upstream work",
    );
    git(&sync, &["switch", "-q", "newgit"]);
    git(
        &sync,
        &["merge", "-q", "--no-ff", "-m", "Sync upstream", "upstream"],
    );
    git(&sync, &["push", "-q", "origin", "newgit"]);
    let before = git(&repo, &["rev-parse", "HEAD"]);

    manager(&store).pull_remote("t3code").expect("pull");
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD^@"]),
        before,
        "the pull is one commit on top"
    );
    assert_eq!(
        git(&repo, &["show", "HEAD:apps/t3code/src/old.ts"]),
        "old",
        "and holds the old upstream work"
    );
    let _ = (old, main);
}

#[test]
fn both_moved_is_a_real_merge_of_the_trackers_main() {
    let (_guard, temp) = tempdir();
    let (store, github, main) = exported(&temp);
    let feature = manager(&store)
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    commit_as(
        &feature,
        "ana",
        1_800_000_000,
        &[("apps/t3code/src/here.ts", "here\n")],
        "t3code: here",
    );
    manager(&store).merge("feature", "main").expect("merge");
    let ours = store_ref(&store, "refs/trackers/t3code/main").expect("main");
    assert_ne!(ours, main);
    let theirs = merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: there",
    );

    let pulled = manager(&store).pull_remote("t3code").expect("pull");
    assert!(pulled.merged);
    let merged = store_ref(&store, "refs/trackers/t3code/main").expect("main");
    let tracker_store = store.paths().tracker_store.clone();
    let parents = git(
        &temp,
        &[
            &format!("--git-dir={tracker_store}"),
            "log",
            "-1",
            "--format=%P",
            &merged,
        ],
    );
    assert_eq!(parents, format!("{ours} {theirs}"));
    let repo = store.paths().project_root.clone();
    assert!(repo.join("apps/t3code/src/pr.ts").is_file());
    assert!(repo.join("apps/t3code/src/here.ts").is_file());
}

#[test]
fn a_history_adding_a_file_a_tracker_outside_git_owns_is_refused() {
    let (_guard, temp) = tempdir();
    let (store, github, main) = exported(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::write(
        repo.join(".newgit/trackers/t3-env.toml"),
        "audience = \"user\"\nstorage = \"local\"\nmount = \"apps/t3code\"\npaths = [\".env\"]\n",
    )
    .expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "t3-env"]);
    merge_pr(
        &temp,
        &github,
        "newgit",
        &[(".env", "SECRET=1\n")],
        "t3: oops",
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let error = manager(&store).pull_remote("t3code").expect_err("refused");
    assert!(error.to_string().contains("`apps/t3code/.env`"), "{error}");
    assert_eq!(
        store_ref(&store, "refs/trackers/t3code/main").as_deref(),
        Some(main.as_str())
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
}

/// A mirror of the project itself, root's, which `access` inherits when
/// `access`.
fn mirrored(temp: &camino::Utf8Path, access: bool) -> (MetadataStore, camino::Utf8PathBuf) {
    let store = project(temp);
    let repo = store.paths().project_root.clone();
    let mirror = temp.join("mirror.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    set_export(&store, "root", Some(&export_to(&mirror, "github", "")));
    set_export(&store, "access", access.then_some("\"inherit\""));
    // Root's too, so the commit is the mirror's even when `access`, which
    // holds the definitions, is withheld there.
    std::fs::write(repo.join("MIRRORED.md"), "mirrored\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export target"]);
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("github", RemoteExportOptions::default())
        .expect("export");
    (store, mirror)
}

#[test]
fn pulling_back_from_a_target_without_root_recognises_our_commits() {
    let (_guard, temp) = tempdir();
    let (store, mirror) = mirrored(&temp, true);
    let repo = store.paths().project_root.clone();
    let theirs = merge_pr(&temp, &mirror, "main", &[("pr.txt", "pr\n")], "from GitHub");

    let pulled = manager(&store).pull_remote("github").expect("pull");
    assert_eq!(pulled.commits, 2);
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        theirs,
        "the very same commits"
    );
    manager(&store).merge("main", "main").expect("merge");
    manager(&store)
        .export_to_remote("github", RemoteExportOptions::default())
        .expect("export");
    assert_eq!(git(&mirror, &["rev-parse", "main"]), theirs);
}

#[test]
fn a_target_withholding_a_tracker_comes_back_with_its_files_put_back() {
    let (_guard, temp) = tempdir();
    let (store, mirror) = mirrored(&temp, false);
    let repo = store.paths().project_root.clone();
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let definitions = git(&repo, &["rev-parse", "HEAD:.newgit/trackers"]);
    merge_pr(&temp, &mirror, "main", &[("pr.txt", "pr\n")], "from GitHub");
    let mirrored_tree = git(&mirror, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(
        !mirrored_tree.contains(".newgit/trackers"),
        "{mirrored_tree}"
    );

    let pulled = manager(&store).pull_remote("github").expect("pull");
    assert_eq!(pulled.commits, 2);
    assert!(
        git_ok(&repo, &["merge-base", "--is-ancestor", &before, "HEAD"]),
        "on top of ours"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD:.newgit/trackers"]),
        definitions
    );
    assert!(repo.join("pr.txt").is_file());
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "HEAD^2"]),
        "from GitHub"
    );
    // It splits like any work, and goes back out as the commits GitHub has.
    manager(&store).merge("main", "main").expect("merge");
    manager(&store)
        .export_to_remote("github", RemoteExportOptions::default())
        .expect("export");
}
