//! `newgit spawn --from-checkpoint` leaves the new instance's workspace on
//! the new instance's own branch (#315). Since #284 a checkpoint records the
//! branch its workspace had checked out, and an undo puts the workspace back
//! on it. The spawn ends in an undo to the imported record, whose `branch` it
//! rewrote for the new instance while `checked_out` stayed the author's, so
//! `review`'s workspace was left on `feature-a` and work done there never
//! reached `review`.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// `newgit spawn review --from-checkpoint feature-a/ckpt_001`, which comes up
/// without moving the workspace off the branch it was made on. Returns
/// `review`'s workspace.
fn spawn_review(fixture: &Fixture) -> Utf8PathBuf {
    let output = fixture.newgit(
        &fixture.store,
        &["spawn", "review", "--from-checkpoint", "feature-a/ckpt_001"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "spawn failed:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("put it back") && !stderr.contains("put it back"),
        "the spawn moved the workspace:\n{stdout}\nstderr:\n{stderr}"
    );
    workspace_of(fixture, "review")
}

/// The branch `workspace` has checked out, `None` when HEAD is detached.
fn checked_out(workspace: &Utf8Path) -> Option<String> {
    let output = git_output(workspace, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `review`'s workspace is on `review`, and `status` and `checkpoints` agree.
fn assert_on_review(fixture: &Fixture, review: &Utf8Path) {
    assert_eq!(checked_out(review).as_deref(), Some("review"));
    let status = fixture.ok(&fixture.store, &["status", "review"]);
    assert!(!status.contains("(workspace "), "{status}");
    let checkpoints = fixture.ok(&fixture.store, &["checkpoints", "review"]);
    assert!(
        !checkpoints.contains("[on `") && !checkpoints.contains("[detached]"),
        "{checkpoints}"
    );
}

#[test]
fn a_spawned_checkpoint_leaves_the_workspace_on_the_new_instances_branch() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let author = workspace_of(&fixture, "feature-a");
    fixture.ok(&author, &["checkpoint"]);

    let review = spawn_review(&fixture);

    assert_on_review(&fixture, &review);
}

#[test]
fn a_commit_in_the_spawned_instance_moves_its_own_branch() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let author = workspace_of(&fixture, "feature-a");
    fixture.ok(&author, &["checkpoint"]);
    let review = spawn_review(&fixture);

    std::fs::write(review.join("review.md"), "reviewed\n").expect("write");
    git(&review, &["add", "-A"]);
    git(&review, &["commit", "-q", "-m", "reviewed"]);
    let head = git(&review, &["rev-parse", "HEAD"]);
    let output = fixture.newgit(&review, &["checkpoint"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "checkpoint failed:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("stays at") && !stderr.contains("stays at"),
        "the checkpoint left `review` behind:\n{stdout}\nstderr:\n{stderr}"
    );

    assert_eq!(
        git(&fixture.store, &["rev-parse", "refs/heads/review"]),
        head,
        "the store's `review` did not follow the checkpoint"
    );
}

/// The author's `try-x` is theirs: the spawn holds what it holds, on `review`.
#[test]
fn an_author_on_another_branch_still_spawns_onto_the_new_instances_branch() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let author = workspace_of(&fixture, "feature-a");
    git(&author, &["checkout", "-q", "-b", "try-x"]);
    std::fs::write(author.join("try-x.md"), "an experiment\n").expect("write");
    git(&author, &["add", "-A"]);
    git(&author, &["commit", "-q", "-m", "try x"]);
    fixture.ok(&author, &["checkpoint"]);

    let review = spawn_review(&fixture);

    assert_on_review(&fixture, &review);
    assert_eq!(
        std::fs::read_to_string(review.join("try-x.md")).expect("read"),
        "an experiment\n"
    );
}

#[test]
fn an_author_detached_still_spawns_onto_the_new_instances_branch() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let author = workspace_of(&fixture, "feature-a");
    git(&author, &["checkout", "-q", "--detach"]);
    fixture.ok(&author, &["checkpoint"]);

    let review = spawn_review(&fixture);

    assert_on_review(&fixture, &review);
}
