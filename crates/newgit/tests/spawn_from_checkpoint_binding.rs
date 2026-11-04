//! `newgit spawn --from-checkpoint` binds what the checkpoint bound: the undo
//! that finishes it restores the checkpoint's binding. It took `--use` and
//! `--drop` and then ignored them — `--drop b` left `b` bound, the author's
//! uncommitted work in it on disk, and exited 0. Both are refused now, as
//! `--from` and `--private` already were, before anything is spawned.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// A code tracker `b` owning `apps/b`, merged into `main` so the remote has
/// it, and `feature-a` spawned with it bound.
fn spawned_with_tracker_b(fixture: &Fixture) -> Utf8PathBuf {
    std::fs::create_dir_all(fixture.store.join("apps/b")).expect("mkdir");
    std::fs::write(fixture.store.join("apps/b/index.js"), "b v1\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "apps/b"]);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "b", "--code", "--path", "apps/b"],
    );
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "b is a tracker"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    workspace_of(fixture, "feature-a")
}

/// `newgit spawn review --from-checkpoint feature-a/ckpt_001 <flag> b`, with
/// and without `--json`: refused as a usage error, with nothing spawned and
/// nothing asked of the remote.
fn assert_refused(fixture: &Fixture, flag: &str) {
    for json in [false, true] {
        let requests = fixture.remote.requests().len();
        let mut args = vec![
            "spawn",
            "review",
            "--from-checkpoint",
            "feature-a/ckpt_001",
            flag,
            "b",
        ];
        if json {
            args.push("--json");
        }
        let output = fixture.newgit(&fixture.store, &args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} was not refused:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(stdout.is_empty(), "{stdout}");
        assert!(
            stderr.contains("cannot be used with")
                && stderr.contains(flag)
                && stderr.contains("--from-checkpoint"),
            "{stderr}"
        );
        assert!(
            !fixture.store.join(".newgit/branches/review.toml").exists(),
            "a record was written for `review`"
        );
        assert!(
            !fixture.temp.join("workspaces/review").exists(),
            "a workspace was made for `review`"
        );
        assert!(
            !git_output(
                &fixture.store,
                &["rev-parse", "--verify", "--quiet", "refs/heads/review"]
            )
            .status
            .success(),
            "a source branch `review` was created"
        );
        assert_eq!(
            fixture.remote.requests().len(),
            requests,
            "the remote was asked something"
        );
    }
}

#[test]
fn drop_with_from_checkpoint_is_refused_rather_than_ignored() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = spawned_with_tracker_b(&fixture);
    std::fs::write(workspace.join("apps/b/index.js"), "the author's edit\n").expect("write");
    fixture.ok(&workspace, &["checkpoint"]);

    assert_refused(&fixture, "--drop");
}

#[test]
fn use_with_from_checkpoint_is_refused_rather_than_ignored() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = spawned_with_tracker_b(&fixture);
    // The checkpoint does not bind `b`.
    fixture.ok(&workspace, &["tracker", "drop", "b"]);
    fixture.ok(&workspace, &["checkpoint"]);

    assert_refused(&fixture, "--use");
}
