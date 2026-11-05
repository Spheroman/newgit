//! A mounted tracker's history stays in its own repo (decided 2026-11-05): a
//! mount joins the view as one ordinary commit, each pull as one more, and no
//! commit of the child is reachable from any view.

mod imports;

use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8Path;
use imports::*;
use newgit_core::compose::{self, ComposeOptions};
use newgit_core::manager::RemoteExportOptions;
use newgit_core::store::MetadataStore;

/// What `git rev-list` lists for `args` in `dir`.
fn commits(dir: &Utf8Path, args: &[&str]) -> BTreeSet<String> {
    let mut full = vec!["rev-list"];
    full.extend_from_slice(args);
    git(dir, &full).lines().map(ToOwned::to_owned).collect()
}

fn assert_no_child_commit(store: &MetadataStore, upstream: &Utf8Path, view: &str) {
    let repo = store.paths().project_root.clone();
    let child = commits(upstream, &["--all"]);
    let reachable = commits(&repo, &[view]);
    assert!(
        child.is_disjoint(&reachable),
        "child commits in the view: {:?}",
        child.intersection(&reachable).collect::<Vec<_>>()
    );
    let subjects = git(&repo, &["log", "--format=%s", view]);
    assert!(!subjects.contains("t3: "), "{subjects}");
}

/// t3code imported and published, with a rooted target a pull can come from.
fn imported(temp: &Utf8Path) -> (MetadataStore, camino::Utf8PathBuf, camino::Utf8PathBuf) {
    let upstream = upstream(temp);
    let store = project(temp);
    let github = github_copy(temp, &upstream, "t3code");
    import(&store, &upstream);
    define_target(
        &store,
        "t3code",
        &format!("url = \"{github}\"\nbranch = \"newgit\"\nroot = \"t3code\"\n"),
    );
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export target"]);
    manager(&store).merge("main", "main").expect("publish");
    (store, upstream, github)
}

#[test]
fn a_mount_joins_as_one_commit_and_a_pull_is_one_more() {
    let (_guard, temp) = tempdir();
    let (store, upstream, github) = imported(&temp);
    let repo = store.paths().project_root.clone();
    assert_no_child_commit(&store, &upstream, "HEAD");
    assert_eq!(
        git(&repo, &["log", "--format=%s", "--", "apps/t3code"]),
        "Import `t3code` at apps/t3code from upstream",
        "`git log` of the mount starts at the import"
    );

    let before = git(&repo, &["rev-parse", "HEAD"]);
    merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: from a PR",
    );
    manager(&store).pull_remote("t3code").expect("pull");
    assert_eq!(git(&repo, &["rev-parse", "HEAD^@"]), before, "one parent");
    assert_eq!(
        git(&repo, &["show", "HEAD:apps/t3code/src/pr.ts"]),
        "pr",
        "its tree holds the child's files at the pin"
    );
    let log = git(&repo, &["log", "--format=%s", "--", "apps/t3code"]);
    let subjects: Vec<&str> = log.lines().collect();
    assert_eq!(subjects.len(), 2, "{log}");
    assert!(subjects[0].starts_with("Pull t3code: 2 commits from "));
    assert!(subjects[1].starts_with("Import `t3code`"));
    assert_no_child_commit(&store, &upstream, "HEAD");
    // Not the commits on GitHub's side either.
    let ours = commits(&repo, &["HEAD"]);
    assert!(ours.is_disjoint(&commits(&github, &["newgit"])));
}

#[test]
fn a_branch_from_before_the_import_has_no_mount_until_it_merges_main() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    let old = manager(&store)
        .spawn("old", None)
        .expect("spawn")
        .branch
        .workspace_path;
    import(&store, &upstream);
    manager(&store).merge("main", "main").expect("publish");
    let repo = store.paths().project_root.clone();
    assert!(!old.join("apps/t3code").exists());

    git(&old, &["config", "commit.gpgSign", "false"]);
    commit_as(&old, "ana", 2_000_000, &[("old.txt", "old\n")], "old work");
    assert!(!old.join("apps/t3code").exists());
    git(&old, &["fetch", "-q", repo.as_str(), "main"]);
    git(&old, &["merge", "-q", "--no-edit", "FETCH_HEAD"]);
    assert!(old.join("apps/t3code/src/app.ts").is_file());

    manager(&store).merge("old", "main").expect("merge");
    let view = git(&repo, &["rev-parse", "HEAD"]);
    assert!(repo.join("old.txt").is_file());
    assert_no_child_commit(&store, &upstream, &view);
}

#[test]
fn work_under_the_mount_round_trips_with_stable_ids() {
    let (_guard, temp) = tempdir();
    let (store, upstream, _) = imported(&temp);
    let repo = store.paths().project_root.clone();
    let feature = manager(&store)
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    git(&feature, &["config", "commit.gpgSign", "false"]);
    let work = commit_as(
        &feature,
        "ana",
        2_000_000,
        &[("apps/t3code/src/app.ts", "ours\n"), ("top.txt", "top\n")],
        "work on both",
    );
    manager(&store).merge("feature", "main").expect("merge");
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        work,
        "the commit itself"
    );
    assert_no_child_commit(&store, &upstream, &work);

    // The child took one commit of its own, on its own history.
    let tracker_store = store.paths().tracker_store.clone();
    let child = store_ref(&store, "refs/trackers/t3code/main").expect("t3code");
    assert_eq!(
        git(
            &repo,
            &[
                &format!("--git-dir={tracker_store}"),
                "log",
                "-1",
                "--format=%s",
                &child,
            ]
        ),
        "work on both"
    );

    // A composer holding only tracker commits gets the same ids.
    let fresh = temp.join("fresh.git");
    git(&temp, &["init", "-q", "--bare", fresh.as_str()]);
    let root = store_ref(&store, "refs/trackers/root/main").expect("root");
    let list = compose::index(&tracker_store, "root", &root).expect("index");
    let mut wanted = BTreeMap::new();
    for (tracker, commit) in &list {
        git(
            &fresh,
            &[
                "fetch",
                "-q",
                tracker_store.as_str(),
                &format!("+refs/trackers/{tracker}/main:refs/trackers/{tracker}/main"),
            ],
        );
        wanted.insert(tracker.clone(), commit.clone());
    }
    let everything = |_: &str| true;
    let views = compose::compose(
        &fresh,
        "root",
        &[("refs/heads/view".to_owned(), root.clone())],
        &ComposeOptions {
            readable: &everything,
            admit: None,
        },
    )
    .expect("compose");
    assert_eq!(views[0].tip.as_deref(), Some(work.as_str()));
    assert_eq!(wanted["root"], root);
}

#[test]
fn a_rooted_export_is_the_trackers_own_history_unchanged() {
    let (_guard, temp) = tempdir();
    let (store, upstream, github) = imported(&temp);
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    assert_eq!(
        git(&github, &["rev-parse", "newgit"]),
        git(&upstream, &["rev-parse", "newgit"]),
    );
}
