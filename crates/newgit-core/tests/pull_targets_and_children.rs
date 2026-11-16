//! `newgit pull` reaches everything "update" means (#426): a tracker with a
//! rooted export target first takes what is new there, and a tracker mounted
//! inside another moves by a commit in the tracker that mounts it, whose pin
//! then moves up to root. No tracker's `main` moves until `newgit merge`. A
//! target that fails holds its tracker, and everything under it, where it
//! is; the rest still move.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

mod imports;

use camino::{Utf8Path, Utf8PathBuf};
use imports::*;
use newgit_core::manager::{BindingChoice, PullOutcome, RemoteExportOptions};
use newgit_core::store::MetadataStore;

/// `t3code` imported at `apps/t3code`, holding `.repos/`, published, and
/// exported to `github` (a bare copy of its upstream, branch `newgit`)
/// rooted at it. Returns the store and `github`.
fn exported(temp: &Utf8Path) -> (MetadataStore, Utf8PathBuf) {
    let upstream = upstream(temp);
    commit_as(
        &upstream,
        "ana",
        1_690_000_000,
        &[(".repos/effect/LLMS.md", "effect\n")],
        "Vendor effect",
    );
    let store = project(temp);
    let github = github_copy(temp, &upstream, "t3code");
    import(&store, &upstream);
    set_export(
        &store,
        "t3code",
        Some(&export_to(&github, "t3code", "branch = \"newgit\"")),
    );
    commit_all(&store, "export target");
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    (store, github)
}

fn commit_all(store: &MetadataStore, message: &str) {
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", message]);
}

/// `apps/t3code/.repos` carved out as `t3code`'s child `repos`.
fn carve_repos(store: &MetadataStore) {
    manager(store)
        .create_git_tracker(
            "repos",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/.repos")],
        )
        .expect("carve");
    manager(store).merge("main", "main").expect("publish");
}

/// `docs/` carved out as a tracker of its own, beside `t3code`.
fn carve_docs(store: &MetadataStore) {
    let repo = store.paths().project_root.clone();
    std::fs::create_dir_all(repo.join("docs")).expect("mkdir");
    std::fs::write(repo.join("docs/index.md"), "docs\n").expect("write");
    commit_all(store, "docs");
    manager(store)
        .create_git_tracker("docs", "project-devs", &[Utf8PathBuf::from("docs")])
        .expect("carve");
    manager(store).merge("main", "main").expect("publish");
}

/// Elsewhere: one commit adding `file` on `tracker`'s `main`, alone.
/// Returns the commit `main` is at now.
fn advance(temp: &Utf8Path, store: &MetadataStore, tracker: &str, file: &str) -> String {
    let tracker_store = store.paths().tracker_store.clone();
    let elsewhere = temp.join(format!("elsewhere-{tracker}-{}", file.replace('/', "-")));
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    git(&elsewhere, &["init", "-q", "-b", "main"]);
    git(&elsewhere, &["config", "commit.gpgSign", "false"]);
    git(
        &elsewhere,
        &[
            "fetch",
            "-q",
            tracker_store.as_str(),
            &format!("+refs/trackers/{tracker}/main:refs/remotes/store/main"),
        ],
    );
    git(
        &elsewhere,
        &["reset", "-q", "--hard", "refs/remotes/store/main"],
    );
    let path = elsewhere.join(file);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "x\n").expect("write");
    git(&elsewhere, &["add", "-A"]);
    git(
        &elsewhere,
        &["commit", "-q", "-m", &format!("{tracker}: {file}")],
    );
    git(
        &elsewhere,
        &[
            "push",
            "-q",
            tracker_store.as_str(),
            &format!("HEAD:refs/trackers/{tracker}/main"),
        ],
    );
    git(&elsewhere, &["rev-parse", "HEAD"])
}

/// The commit a tracker's commit `commit` (any rev the tracker store
/// resolves) pins `name` at.
fn pin_of(store: &MetadataStore, commit: &str, name: &str) -> String {
    let tracker_store = store.paths().tracker_store.clone();
    git(
        &store.paths().project_root,
        &[
            &format!("--git-dir={tracker_store}"),
            "rev-parse",
            &format!("{commit}:.newgit/pins/{name}"),
        ],
    )
}

fn main_of(store: &MetadataStore, tracker: &str) -> String {
    store_ref(store, &format!("refs/trackers/{tracker}/main"))
        .unwrap_or_else(|| panic!("`{tracker}` has a main"))
}

fn moved(outcome: &PullOutcome) -> Vec<(&str, usize)> {
    outcome
        .pulled
        .iter()
        .map(|pulled| (pulled.tracker.as_str(), pulled.commits))
        .collect()
}

#[test]
fn plain_pull_takes_what_is_new_on_a_rooted_target_and_leaves_main_to_merge() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let main = main_of(&store, "t3code");
    let theirs = merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: from a PR",
    );
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let pulled = manager(&store).pull(None, &[]).expect("pull");
    assert_eq!(moved(&pulled), [("t3code", 2)]);
    assert!(
        pulled.pulled[0].from.starts_with("main and "),
        "{:?}",
        pulled.pulled[0].from
    );
    let view = pulled.view.expect("a view commit");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), view);
    assert_eq!(git(&repo, &["rev-parse", "HEAD^@"]), before, "one commit");
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Update trackers: t3code (2 commits)"
    );
    assert!(repo.join("apps/t3code/src/pr.ts").is_file());
    assert_eq!(
        pin_of(&store, "refs/trackers/root/instances/main", "t3code"),
        theirs
    );
    assert_eq!(
        store_ref(&store, "refs/trackers/t3code/instances/main").as_deref(),
        Some(theirs.as_str()),
        "the commits ride on this instance's branch of t3code"
    );
    assert_eq!(main_of(&store, "t3code"), main, "`main` waits for a merge");

    // Again: nothing new there, and nothing written.
    let again = manager(&store).pull(None, &[]).expect("again");
    assert_eq!(again.view, None, "{again:?}");
    assert!(again.up_to_date.contains(&"t3code".to_owned()));

    manager(&store).merge("main", "main").expect("merge");
    assert_eq!(main_of(&store, "t3code"), theirs, "the merge lands it");
}

#[test]
fn inside_an_instance_only_its_pin_moves_until_it_merges() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let main = main_of(&store, "t3code");
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
    let root_before = git(&repo, &["rev-parse", "HEAD"]);

    let pulled = manager(&store).pull(Some("feature"), &[]).expect("pull");
    assert_eq!(moved(&pulled), [("t3code", 2)]);
    assert_eq!(
        git(&feature, &["rev-parse", "HEAD"]),
        pulled.view.expect("view")
    );
    assert!(feature.join("apps/t3code/src/pr.ts").is_file());
    assert_eq!(
        pin_of(&store, "refs/trackers/root/instances/feature", "t3code"),
        theirs
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), root_before);
    assert!(!repo.join("apps/t3code/src/pr.ts").exists());
    assert_eq!(main_of(&store, "t3code"), main, "shared: it waits");
    assert_eq!(pin_of(&store, "refs/trackers/root/main", "t3code"), main);

    manager(&store).merge("feature", "main").expect("merge");
    assert_eq!(main_of(&store, "t3code"), theirs);
    assert_eq!(pin_of(&store, "refs/trackers/root/main", "t3code"), theirs);
}

#[test]
fn both_moved_is_a_merge_and_a_conflict_skips_only_that_tracker() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    carve_docs(&store);
    let repo = store.paths().project_root.clone();
    // t3code's `main` and the target both change `src/app.ts`.
    let feature = manager(&store)
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    commit_as(
        &feature,
        "ana",
        1_800_000_000,
        &[("apps/t3code/src/app.ts", "ours\n")],
        "t3code: ours",
    );
    manager(&store).merge("feature", "main").expect("merge");
    merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/app.ts", "theirs\n")],
        "t3: theirs",
    );
    let docs = advance(&temp, &store, "docs", "more.md");
    let main_before = main_of(&store, "t3code");

    let pulled = manager(&store).pull(None, &[]).expect("pull");
    assert_eq!(moved(&pulled), [("docs", 1)], "docs still moves");
    let [(tracker, target, reason)] = pulled.target_failed.as_slice() else {
        panic!("one target failed: {pulled:?}");
    };
    assert_eq!((tracker.as_str(), target.as_str()), ("t3code", "t3code"));
    assert!(reason.contains("conflicts"), "{reason}");
    assert!(reason.contains("src/app.ts"), "{reason}");
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Update trackers: docs (1 commit)"
    );
    assert_eq!(
        pin_of(&store, "refs/trackers/root/instances/main", "docs"),
        docs
    );
    assert_eq!(
        pin_of(&store, "refs/trackers/root/instances/main", "t3code"),
        main_before,
        "nothing half-moved"
    );
    assert_eq!(main_of(&store, "t3code"), main_before);
    assert_eq!(
        std::fs::read_to_string(repo.join("apps/t3code/src/app.ts")).expect("read"),
        "ours\n"
    );
}

#[test]
fn a_target_that_both_moved_without_conflict_is_merged_into_the_pin() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let ours = advance(&temp, &store, "t3code", "src/here.ts");
    let theirs = merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: there",
    );

    let pulled = manager(&store).pull(None, &[]).expect("pull");
    assert_eq!(pulled.pulled.len(), 1, "{pulled:?}");
    let pin = pin_of(&store, "refs/trackers/root/instances/main", "t3code");
    let parents = git(&repo, &["log", "-1", "--format=%P", &pin]);
    assert_eq!(parents, format!("{ours} {theirs}"), "a real merge");
    assert!(repo.join("apps/t3code/src/here.ts").is_file());
    assert!(repo.join("apps/t3code/src/pr.ts").is_file());
    assert_eq!(main_of(&store, "t3code"), ours, "`main` waits for a merge");

    let again = manager(&store).pull(None, &[]).expect("again");
    assert_eq!(again.view, None, "the merge is not made twice: {again:?}");
}

#[test]
fn an_unreachable_target_is_named_and_its_tracker_stays() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    import(&store, &upstream);
    set_export(
        &store,
        "t3code",
        Some(&export_to(&temp.join("gone.git"), "t3code", "")),
    );
    commit_all(&store, "export target");
    manager(&store).merge("main", "main").expect("publish");
    carve_docs(&store);
    advance(&temp, &store, "t3code", "src/later.ts");
    let docs = advance(&temp, &store, "docs", "more.md");
    let pin_before = pin_of(&store, "refs/trackers/root/instances/main", "t3code");

    let pulled = manager(&store).pull(None, &[]).expect("pull");
    assert_eq!(moved(&pulled), [("docs", 1)]);
    assert_eq!(pulled.target_failed.len(), 1, "{pulled:?}");
    assert_eq!(pulled.target_failed[0].0, "t3code");
    assert_eq!(
        pin_of(&store, "refs/trackers/root/instances/main", "docs"),
        docs
    );
    assert_eq!(
        pin_of(&store, "refs/trackers/root/instances/main", "t3code"),
        pin_before,
        "its `main` moved, but without its target it is not up to date: it stays"
    );
}

#[test]
fn a_child_moves_by_a_commit_in_the_tracker_that_mounts_it() {
    let (_guard, temp) = tempdir();
    let (store, _) = exported(&temp);
    carve_repos(&store);
    let repo = store.paths().project_root.clone();
    let parent = pin_of(&store, "refs/trackers/root/instances/main", "t3code");
    let parent_main = main_of(&store, "t3code");
    let child = advance(&temp, &store, "t3code__repos", "effect/new.md");

    let pulled = manager(&store).pull(None, &[]).expect("pull");
    assert_eq!(moved(&pulled), [("t3code__repos", 1)]);
    let [(mounting, made)] = pulled.mounting.as_slice() else {
        panic!("one commit in the tracker that mounts it: {pulled:?}");
    };
    assert_eq!(mounting, "t3code");
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Update trackers: t3code/repos (1 commit)",
        "root's commit names it by its path"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s %P", made]),
        format!("Update trackers: repos (1 commit) {parent}"),
        "t3code's names it from there, on its pin"
    );
    assert_eq!(pin_of(&store, made, "repos"), child);
    assert_eq!(
        pin_of(&store, "refs/trackers/root/instances/main", "t3code"),
        *made
    );
    assert_eq!(
        store_ref(&store, "refs/trackers/t3code/instances/main").as_deref(),
        Some(made.as_str())
    );
    assert!(repo.join("apps/t3code/.repos/effect/new.md").is_file());
    assert_eq!(main_of(&store, "t3code"), parent_main);

    manager(&store).merge("main", "main").expect("merge");
    assert_eq!(main_of(&store, "t3code"), *made);
    assert_eq!(main_of(&store, "t3code__repos"), child);
}

#[test]
fn naming_a_child_pulls_only_it_and_naming_its_parent_does_not_reach_it() {
    let (_guard, temp) = tempdir();
    let (store, _) = exported(&temp);
    carve_repos(&store);
    carve_docs(&store);
    advance(&temp, &store, "t3code__repos", "effect/new.md");
    advance(&temp, &store, "docs", "more.md");

    let parent = manager(&store)
        .pull(None, &["t3code".to_owned()])
        .expect("pull t3code");
    assert_eq!(parent.view, None, "{parent:?}");
    assert_eq!(parent.up_to_date, ["t3code"]);

    let child = manager(&store)
        .pull(None, &["t3code__repos".to_owned()])
        .expect("pull t3code/repos");
    assert_eq!(moved(&child), [("t3code__repos", 1)]);
    assert_eq!(child.mounting.len(), 1);
    let repo = store.paths().project_root.clone();
    assert!(repo.join("apps/t3code/.repos/effect/new.md").is_file());
    assert!(!repo.join("docs/more.md").exists(), "docs was not named");

    let both = manager(&store)
        .pull(None, &["docs".to_owned(), "t3code__repos".to_owned()])
        .expect("pull both");
    assert_eq!(moved(&both), [("docs", 1)]);
    assert!(both.up_to_date.contains(&"t3code__repos".to_owned()));
}

#[test]
fn naming_what_is_not_a_mounted_tracker_in_git_is_an_error() {
    let (_guard, temp) = tempdir();
    let (store, _) = exported(&temp);
    let error = manager(&store)
        .pull(None, &["nope".to_owned()])
        .expect_err("not a tracker");
    assert!(
        error.to_string().contains("`nope` is not a tracker in Git"),
        "{error}"
    );
}

#[test]
fn a_child_of_a_tracker_this_instance_does_not_bind_stays_and_is_named() {
    let (_guard, temp) = tempdir();
    let (store, _) = exported(&temp);
    carve_repos(&store);
    manager(&store)
        .spawn_binding(
            "without",
            None,
            &BindingChoice {
                uses: Vec::new(),
                drops: vec!["t3code".to_owned()],
                private: false,
            },
        )
        .expect("spawn");
    advance(&temp, &store, "t3code__repos", "effect/new.md");

    let pulled = manager(&store).pull(Some("without"), &[]).expect("pull");
    assert_eq!(pulled.view, None, "{pulled:?}");
    assert_eq!(
        pulled.unbound,
        [("t3code__repos".to_owned(), "t3code".to_owned())]
    );
}
