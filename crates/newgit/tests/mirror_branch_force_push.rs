//! A force-push from a workspace replaces the instance's branch on a mirror
//! only over what newgit itself last put there.
//!
//! The lease is newgit's record of its last push of the branch
//! (`refs/newgit/exports/<target>/pushed-branches/<slug>`, see
//! `mirror_branch_lease.rs`). A `git push --force` in the workspace rewrites
//! the branch on the remote, and its export follows over that record and
//! nothing else: a commit someone pushed to the mirror's branch stays, and
//! with no record the export is refused. Only `newgit export <instance>
//! --remote <target> --force` replaces the branch whatever it holds.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");
include!("support/mirror.rs");

const RECORD: &str = "refs/newgit/exports/github/pushed-branches/feature";

/// Someone pushes a commit to `feature` on the mirror, from a clone in
/// `dir`. Returns it.
fn someone_pushes(fixture: &Fixture, github: &Utf8Path, dir: &str) -> String {
    let other = fixture.temp.join(dir);
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

/// Reword the workspace's last commit and force-push it. The push itself
/// must succeed whatever its export does; returns what it said.
fn amend_and_force_push(workspace: &Utf8Path) -> String {
    git(
        workspace,
        &["commit", "-q", "--amend", "-m", "Add first.txt, reworded"],
    );
    let pushed = git_output(workspace, &["push", "--force", "origin", "feature"]);
    assert!(
        pushed.status.success(),
        "the push to origin fails only for itself: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    )
}

/// `feature`, spawned in the project root and pushed once, so the mirror
/// holds it and newgit has its record. Returns the workspace.
fn exported_feature(fixture: &Fixture) -> Utf8PathBuf {
    fixture.ok(&fixture.store, &["spawn", "feature"]);
    let workspace = workspace_of(fixture, "feature");
    commit_and_push(&workspace, "first.txt");
    workspace
}

/// The regression: a workspace force-push used to become a blind `+` push
/// on the mirror, throwing away a commit someone pushed there.
#[test]
fn a_force_push_from_the_workspace_keeps_a_commit_someone_pushed_there() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let elsewhere = another_copy(&fixture);
    merged_on_the_mirror(&fixture, &github);
    let workspace = spawned_on_fetched_main(&fixture, &elsewhere);
    commit_and_push(&workspace, "first.txt");
    let held = someone_pushes(&fixture, &github, "someone");

    let said = amend_and_force_push(&workspace);
    let now = git(&github, &["rev-parse", "feature"]);
    assert!(
        descends(&github, &held, &now),
        "someone's commit {held} was thrown away by a workspace force-push; the mirror has \
         {now}:\n{said}"
    );
    assert!(said.contains("left as it is"), "the push says so: {said}");
    assert!(
        said.contains("newgit export feature --remote github --force"),
        "the refusal names the way out: {said}"
    );
}

/// With nobody else's commit there, a force-push replaces the branch: the
/// record is exactly what the mirror holds.
#[test]
fn a_force_push_from_the_workspace_replaces_what_newgit_put_there() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let workspace = exported_feature(&fixture);
    let before = git(&github, &["rev-parse", "feature"]);

    let said = amend_and_force_push(&workspace);
    assert!(!said.contains("left as it is"), "{said}");
    assert!(said.contains("replacing"), "the push says so: {said}");
    assert_ne!(git(&github, &["rev-parse", "feature"]), before);
    assert_eq!(own_commits(&github), ["Add first.txt, reworded"]);
    assert_eq!(
        git(&fixture.store, &["rev-parse", RECORD]),
        git(&github, &["rev-parse", "feature"]),
        "the record is what was pushed"
    );
}

/// With no record (exported from another machine, or by a newgit from before
/// the record), a force-push does not replace the branch, and says that
/// newgit has no record rather than that someone pushed there.
#[test]
fn a_force_push_with_no_record_is_refused_and_says_there_is_none() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let workspace = exported_feature(&fixture);
    let held = git(&github, &["rev-parse", "feature"]);
    git(&fixture.store, &["update-ref", "-d", RECORD]);

    let said = amend_and_force_push(&workspace);
    assert_eq!(git(&github, &["rev-parse", "feature"]), held, "{said}");
    assert!(said.contains("no record"), "{said}");
    assert!(!said.contains("someone pushed"), "{said}");
    assert!(
        said.contains("newgit export feature --remote github --force"),
        "{said}"
    );

    // Which replaces it, when its owner has looked.
    fixture.ok(
        &fixture.store,
        &["export", "feature", "--remote", "github", "--force"],
    );
    assert_eq!(own_commits(&github), ["Add first.txt, reworded"]);
}

/// The no-record refusal of a heal (`newgit export <instance> --remote`, no
/// `--force`) says the same.
#[test]
fn exporting_again_with_no_record_says_there_is_none() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (elsewhere, _, _) = exported_on_a_copy_of_main(&fixture, &github);
    git(&elsewhere, &["update-ref", "-d", RECORD]);
    let held = git(&github, &["rev-parse", "feature"]);

    let output = fixture.newgit(&elsewhere, &["export", "feature", "--remote", "github"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "it should have been refused");
    assert!(stderr.contains("no record"), "{stderr}");
    assert!(
        stderr.contains("Look at what `feature` there holds"),
        "{stderr}"
    );
    assert!(!stderr.contains("someone pushed"), "{stderr}");
    assert_eq!(git(&github, &["rev-parse", "feature"]), held);
}

/// `newgit remove` drops the instance's record: an instance spawned again
/// under the name is another instance, and a force-push of its may not
/// replace what the first one left on the mirror over the first one's lease.
#[test]
fn removing_an_instance_drops_its_record() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    exported_feature(&fixture);
    assert_eq!(
        git(&fixture.store, &["rev-parse", "--verify", RECORD]),
        git(&github, &["rev-parse", "feature"])
    );

    fixture.ok(&fixture.store, &["remove", "feature"]);
    assert!(
        !git_output(&fixture.store, &["rev-parse", "--verify", "-q", RECORD])
            .status
            .success(),
        "the record outlived its instance"
    );
}
