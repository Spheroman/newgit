//! A mirror holds only what it ships. With `trackers` set, exactly those
//! trackers go out: a tracker it leaves out, of any audience, leaves no path,
//! no pin, and no history on the target, and a commit that only moved its pin
//! is not shown at all.

mod imports;

use camino::{Utf8Path, Utf8PathBuf};
use imports::*;
use newgit_core::manager::RemoteExportOptions;
use newgit_core::store::MetadataStore;

fn export(store: &MetadataStore, target: &str) {
    manager(store)
        .export_to_remote(target, RemoteExportOptions::default())
        .expect("export");
}

/// A project with t3code mounted (`public`: it is an open fork), a mirror
/// shipping `root` and `access`, and a rooted target to pull t3code from.
fn mirrored(temp: &Utf8Path) -> (MetadataStore, Utf8PathBuf, Utf8PathBuf, Utf8PathBuf) {
    let upstream = upstream(temp);
    let store = project(temp);
    let t3code = github_copy(temp, &upstream, "t3code");
    let mirror = temp.join("mirror.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    manager(&store)
        .import_git_tracker(
            "t3code",
            "public",
            Utf8Path::new("apps/t3code"),
            upstream.as_str(),
            Some("newgit"),
        )
        .expect("import");
    define_target(
        &store,
        "github",
        &format!("url = \"{mirror}\"\ntrackers = [\"root\", \"access\"]\n"),
    );
    define_target(
        &store,
        "t3code",
        &format!("url = \"{t3code}\"\nbranch = \"newgit\"\nroot = \"t3code\"\n"),
    );
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export targets"]);
    manager(&store).merge("main", "main").expect("publish");
    export(&store, "github");
    (store, upstream, t3code, mirror)
}

fn assert_holds_nothing_of_t3code(mirror: &Utf8Path, upstream: &Utf8Path) {
    let files = git(mirror, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!files.contains("apps/t3code"), "{files}");
    assert!(!files.contains(".newgit/pins"), "{files}");
    let shown = git(
        mirror,
        &[
            "log",
            "--format=%s",
            "main",
            "--",
            "apps/t3code",
            ".newgit/pins",
        ],
    );
    assert_eq!(shown, "", "no commit ever touched them");
    let ours: Vec<String> = git(mirror, &["rev-list", "main"])
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    for commit in git(upstream, &["rev-list", "--all"]).lines() {
        assert!(!ours.iter().any(|id| id == commit), "{commit} is on it");
    }
    let subjects = git(mirror, &["log", "--format=%s", "main"]);
    assert!(!subjects.contains("t3: "), "{subjects}");
}

#[test]
fn a_tracker_left_out_leaves_no_path_pin_or_history_even_when_public() {
    let (_guard, temp) = tempdir();
    let (store, upstream, _, mirror) = mirrored(&temp);
    assert_holds_nothing_of_t3code(&mirror, &upstream);
    // Its definition is `access`'s, which ships.
    let definition = git(&mirror, &["show", "main:.newgit/trackers/t3code.toml"]);
    assert!(definition.contains("apps/t3code"), "{definition}");
    assert!(
        store
            .paths()
            .project_root
            .join("apps/t3code/src/app.ts")
            .is_file(),
        "the project keeps it"
    );
}

#[test]
fn a_target_naming_no_trackers_still_ships_the_public_ones() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    let mirror = temp.join("public.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    manager(&store)
        .import_git_tracker(
            "t3code",
            "public",
            Utf8Path::new("apps/t3code"),
            upstream.as_str(),
            Some("newgit"),
        )
        .expect("import");
    define_target(
        &store,
        "open",
        &format!("url = \"{mirror}\"\nroot = \"t3code\"\n"),
    );
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "target"]);
    manager(&store).merge("main", "main").expect("publish");
    export(&store, "open");
    assert!(!git(&mirror, &["ls-tree", "-r", "--name-only", "main"]).is_empty());
}

#[test]
fn a_commit_that_only_moved_a_withheld_pin_is_dropped() {
    let (_guard, temp) = tempdir();
    let (store, upstream, t3code, mirror) = mirrored(&temp);
    let before = git(&mirror, &["rev-parse", "main"]);
    merge_pr(
        &temp,
        &t3code,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: from a PR",
    );
    manager(&store).pull_remote("t3code").expect("pull");
    let repo = store.paths().project_root.clone();
    assert!(git(&repo, &["log", "-1", "--format=%s"]).starts_with("Pull t3code"));
    manager(&store).merge("main", "main").expect("publish");
    export(&store, "github");
    assert_eq!(
        git(&mirror, &["rev-parse", "main"]),
        before,
        "nothing to show: the mirror does not move"
    );
    assert_holds_nothing_of_t3code(&mirror, &upstream);
}

#[test]
fn a_merge_joining_only_a_withheld_pin_is_dropped_too() {
    let (_guard, temp) = tempdir();
    let (store, upstream, t3code, mirror) = mirrored(&temp);
    let feature = manager(&store)
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    git(&feature, &["config", "commit.gpgSign", "false"]);
    commit_as(
        &feature,
        "ana",
        2_000_000,
        &[("feature.txt", "feature\n")],
        "feature work",
    );
    // The project root pulls t3code while the feature lands, so `main` has
    // to merge the two, and the side holding the pull shows nothing here.
    merge_pr(
        &temp,
        &t3code,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: from a PR",
    );
    manager(&store).pull_remote("t3code").expect("pull");
    manager(&store).merge("feature", "main").expect("feature");
    manager(&store).merge("main", "main").expect("the pull");
    export(&store, "github");

    assert_eq!(
        git(&mirror, &["log", "--format=%s", "-1", "main"]),
        "feature work",
        "the merge shows nothing, so it is not shown"
    );
    assert_eq!(git(&mirror, &["rev-list", "--merges", "main"]), "");
    assert_holds_nothing_of_t3code(&mirror, &upstream);
}
