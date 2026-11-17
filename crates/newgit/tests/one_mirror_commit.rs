//! One commit, one commit on a mirror, whichever export makes it first.
//!
//! A commit merged on a mirror lands on the remote as a root commit naming the
//! mirror's own (`landed_view_not_split_again.rs`). A copy that starts an
//! instance on the remote's `main` as it *fetched* it, never pulled, used to
//! split that commit again, into a root commit of its own that had lost the
//! original. The instance's branch then reached the mirror with a second copy
//! of what the mirror's `main` already held, under another id, and every
//! commit above it with it: a pull request showing commits that are not its
//! own, by an author nobody is.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");
include!("support/mirror.rs");

#[test]
fn a_branch_exported_before_main_is_the_mirrors_main_plus_its_own_commits() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let elsewhere = another_copy(&fixture);
    let theirs = merged_on_the_mirror(&fixture, &github);

    let workspace = spawned_on_fetched_main(&fixture, &elsewhere);
    commit_and_push(&workspace, "mine.txt");
    assert_eq!(
        git(&github, &["rev-parse", "feature^"]),
        theirs,
        "the branch is built on the mirror's own commit:\n{}",
        git(&github, &["log", "--format=%h %p %s", "-4", "feature"])
    );
    assert_eq!(own_commits(&github), ["Add mine.txt"]);

    // Then this copy's `main`: the same commit, not another one.
    git_as_user(
        &fixture,
        &elsewhere,
        &["pull", "-q", "--ff-only", "origin", "main"],
    );
    fixture.ok(&elsewhere, &["export", "--remote", "github"]);
    assert_eq!(git(&github, &["rev-parse", "main"]), theirs);
    assert_eq!(own_commits(&github), ["Add mine.txt"]);
}

#[test]
fn a_branch_exported_after_main_adds_only_its_own_commits() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let elsewhere = another_copy(&fixture);
    let theirs = merged_on_the_mirror(&fixture, &github);

    git_as_user(
        &fixture,
        &elsewhere,
        &["pull", "-q", "--ff-only", "origin", "main"],
    );
    fixture.ok(&elsewhere, &["export", "--remote", "github"]);
    assert_eq!(git(&github, &["rev-parse", "main"]), theirs);

    let workspace = spawned_on_fetched_main(&fixture, &elsewhere);
    commit_and_push(&workspace, "mine.txt");
    assert_eq!(git(&github, &["rev-parse", "feature^"]), theirs);
    assert_eq!(own_commits(&github), ["Add mine.txt"]);
}

#[test]
fn a_branch_built_on_a_copy_is_rebuilt_on_the_mirrors_own_at_its_next_push() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (_, workspace, theirs) = exported_on_a_copy_of_main(&fixture, &github);
    let before = git(&workspace, &["rev-parse", "HEAD"]);

    let said = commit_and_push(&workspace, "second.txt");
    assert_eq!(
        git(&github, &["rev-parse", "feature~2"]),
        theirs,
        "{}",
        git(&github, &["log", "--format=%h %p %s", "-5", "feature"])
    );
    assert_eq!(own_commits(&github), ["Add first.txt", "Add second.txt"]);
    assert!(said.contains("replacing"), "the push says so: {said}");
    // The workspace's own commits are what they were: only the mirror's
    // differ.
    assert_eq!(git(&workspace, &["rev-parse", "HEAD^"]), before);
    assert_eq!(
        git(&workspace, &["rev-parse", "refs/remotes/github/feature"]),
        git(&github, &["rev-parse", "feature"])
    );
}

#[test]
fn exporting_the_instance_again_rebuilds_it_without_a_new_commit() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (elsewhere, workspace, theirs) = exported_on_a_copy_of_main(&fixture, &github);

    let stdout = fixture.ok(&elsewhere, &["export", "feature", "--remote", "github"]);
    assert!(stdout.contains("replacing"), "{stdout}");
    assert_eq!(git(&github, &["rev-parse", "feature^"]), theirs);
    assert_eq!(own_commits(&github), ["Add first.txt"]);
    assert_eq!(
        git(&workspace, &["rev-parse", "refs/remotes/github/feature"]),
        git(&github, &["rev-parse", "feature"])
    );

    // Nothing to do the second time.
    let again = fixture.ok(&elsewhere, &["export", "feature", "--remote", "github"]);
    assert!(!again.contains("replacing"), "{again}");
    assert_eq!(own_commits(&github), ["Add first.txt"]);
}

/// The mirror's branch is replaced only while it is what newgit last put
/// there: a commit someone pushed to it straight is never thrown away.
#[test]
fn a_branch_someone_pushed_to_on_the_mirror_is_left_as_it_is() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (elsewhere, _, _) = exported_on_a_copy_of_main(&fixture, &github);

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
    let held = git(&github, &["rev-parse", "feature"]);

    let output = fixture.newgit(&elsewhere, &["export", "feature", "--remote", "github"]);
    assert!(!output.status.success(), "it should have been refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("left as it is"), "{stderr}");
    assert_eq!(git(&github, &["rev-parse", "feature"]), held);
}
