//! An instance's branch on a mirror is replaced only over what newgit itself
//! last put there.
//!
//! When an instance's commits are split again onto the remote's root
//! commits (`one_mirror_commit.rs`), its branch on a `branches = true`
//! target no longer descends from what was exported before, and is
//! replaced. The lease that makes that safe is newgit's own record of its
//! last push, kept in the project repo: never the workspace's
//! `refs/remotes/<target>/<branch>`, which any `git fetch` of the target
//! rewrites with whatever is there, someone else's commit included.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");
include!("support/mirror.rs");

/// Someone pushes a commit to `feature` on the mirror. Returns it.
fn someone_pushes(fixture: &Fixture, github: &Utf8Path) -> String {
    let other = fixture.temp.join("someone");
    git(
        &fixture.temp,
        &[
            "clone",
            "-q",
            "-b",
            "feature",
            github.as_str(),
            other.as_str(),
        ],
    );
    std::fs::write(other.join("theirs.txt"), "theirs\n").expect("write");
    git(&other, &["add", "-A"]);
    git(
        &other,
        &["commit", "-q", "-m", "Pushed straight to the mirror"],
    );
    git(&other, &["push", "-q", "origin", "feature"]);
    git(github, &["rev-parse", "feature"])
}

/// The agent fetches the mirror, as anyone with a `github` remote does,
/// then commits and pushes. Its workspace's record of the mirror's branch is
/// then the stranger's commit, which must not become the lease.
#[test]
fn fetching_the_mirror_then_pushing_keeps_a_commit_someone_pushed_there() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let elsewhere = another_copy(&fixture);
    merged_on_the_mirror(&fixture, &github);
    let workspace = spawned_on_fetched_main(&fixture, &elsewhere);
    commit_and_push(&workspace, "first.txt");

    let held = someone_pushes(&fixture, &github);
    git(&workspace, &["fetch", "-q", "github"]);
    assert_eq!(
        git(&workspace, &["rev-parse", "refs/remotes/github/feature"]),
        held,
        "the state this test is about: the workspace's record is the stranger's"
    );

    let said = commit_and_push(&workspace, "second.txt");
    assert_eq!(
        git(&github, &["rev-parse", "feature"]),
        held,
        "someone's commit was thrown away:\n{said}"
    );
    assert!(said.contains("left as it is"), "the push says so: {said}");
    assert!(!said.contains("replacing"), "{said}");
}

/// The same through `newgit export <instance> --remote`, on a branch that
/// does need rebuilding: the rebuild waits rather than take the commit.
#[test]
fn fetching_the_mirror_then_exporting_again_keeps_a_commit_someone_pushed_there() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (elsewhere, workspace, _) = exported_on_a_copy_of_main(&fixture, &github);
    let held = someone_pushes(&fixture, &github);
    git(&workspace, &["fetch", "-q", "github"]);

    let output = fixture.newgit(&elsewhere, &["export", "feature", "--remote", "github"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "it should have been refused");
    assert!(stderr.contains("left as it is"), "{stderr}");
    assert!(
        stderr.contains("newgit export feature --remote github --force"),
        "the refusal names the way out: {stderr}"
    );
    assert_eq!(git(&github, &["rev-parse", "feature"]), held);

    // Which replaces it, when that is what its owner wants.
    fixture.ok(
        &elsewhere,
        &["export", "feature", "--remote", "github", "--force"],
    );
    assert_eq!(own_commits(&github), ["Add first.txt"]);
}

/// The mirror's branch was deleted (its pull request closed with the
/// branch): the branch is pushed again, rebuilt, not refused forever.
#[test]
fn a_branch_deleted_on_the_mirror_is_pushed_again() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (_, workspace, theirs) = exported_on_a_copy_of_main(&fixture, &github);
    git(&github, &["update-ref", "-d", "refs/heads/feature"]);

    let said = commit_and_push(&workspace, "second.txt");
    assert!(!said.contains("warning"), "{said}");
    assert_eq!(git(&github, &["rev-parse", "feature~2"]), theirs);
    assert_eq!(own_commits(&github), ["Add first.txt", "Add second.txt"]);

    // And it is newgit's again: the next push adds to it.
    let said = commit_and_push(&workspace, "third.txt");
    assert!(!said.contains("warning"), "{said}");
    assert_eq!(
        own_commits(&github),
        ["Add first.txt", "Add second.txt", "Add third.txt"]
    );
}

/// A workspace made again, with no record of the mirror's branch, still
/// heals: the record is the project's, not the workspace's.
#[test]
fn a_workspace_without_its_record_of_the_mirror_still_heals() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (_, workspace, theirs) = exported_on_a_copy_of_main(&fixture, &github);
    git(
        &workspace,
        &["update-ref", "-d", "refs/remotes/github/feature"],
    );

    let said = commit_and_push(&workspace, "second.txt");
    assert!(said.contains("replacing"), "{said}");
    assert_eq!(git(&github, &["rev-parse", "feature~2"]), theirs);
    assert_eq!(own_commits(&github), ["Add first.txt", "Add second.txt"]);
}

/// `newgit export <instance> --remote` heals the instance's tracker
/// branches with its branch on the mirror, so merging it afterwards brings
/// no copy of the mirror's commit into `main`.
#[test]
fn exporting_again_heals_the_instances_tracker_branch_too() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (elsewhere, _, theirs) = exported_on_a_copy_of_main(&fixture, &github);
    let instance_root = "refs/newgit/trackers/root/instances/feature";
    let copy = git(&elsewhere, &["rev-parse", &format!("{instance_root}^")]);

    fixture.ok(&elsewhere, &["export", "feature", "--remote", "github"]);
    assert!(
        !descends(&elsewhere, &copy, instance_root),
        "the instance's root branch still holds the copy:\n{}",
        git(
            &elsewhere,
            &["log", "--format=%h %p %s", "-4", instance_root]
        )
    );

    fixture.ok(&elsewhere, &["merge", "feature"]);
    assert!(!descends(
        &elsewhere,
        &copy,
        "refs/newgit/trackers/root/main"
    ));
    fixture.ok(&elsewhere, &["export", "--remote", "github"]);
    assert_eq!(git(&github, &["rev-parse", "main^"]), theirs);
}

/// When the target's tracker set changed, every commit of the branch there
/// would change: that is refused, as `main`'s export is without `--force`.
#[test]
fn a_changed_tracker_set_is_not_a_reason_to_replace_the_branch() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    fixture.ok(&fixture.store, &["spawn", "feature"]);
    let workspace = workspace_of(&fixture, "feature");
    commit_and_push(&workspace, "first.txt");
    let held = git(&github, &["rev-parse", "feature"]);

    // `private` ships from now on.
    set_export(&fixture.store, "private", Some("\"inherit\""));
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "ship private"]);
    fixture.ok(&fixture.store, &["merge", "main"]);

    let output = fixture.newgit(&fixture.store, &["export", "feature", "--remote", "github"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "it should have been refused");
    assert!(stderr.contains("trackers are not the ones"), "{stderr}");
    assert_eq!(git(&github, &["rev-parse", "feature"]), held);

    fixture.ok(
        &fixture.store,
        &["export", "feature", "--remote", "github", "--force"],
    );
    assert_ne!(git(&github, &["rev-parse", "feature"]), held);
    git(&github, &["cat-file", "-e", "feature:private/x.txt"]);
}
