//! Regression tests for #208: the human-readable `newgit spawn
//! --from-checkpoint` exited 0 when a restore failed, and its `resource:`
//! line named what the restore did without saying whether it worked. The
//! `--json` form already exited 1 on `SpawnFromCheckpointOutcome::is_complete`
//! (`spawn_from_checkpoint_json.rs`); this form now judges by the same
//! condition and prints each restore's verdict the way `undo` does.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// `feature-a`, spawned and checkpointed as `feature-a/ckpt_001`.
fn checkpointed(fixture: &Fixture) {
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(fixture, "feature-a");
    fixture.ok(&workspace, &["checkpoint"]);
}

struct Spawned {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// `review`, spawned from `feature-a/ckpt_001` without `--json`.
fn spawn_review(fixture: &Fixture) -> Spawned {
    let output = fixture.newgit(
        &fixture.store,
        &["spawn", "review", "--from-checkpoint", "feature-a/ckpt_001"],
    );
    Spawned {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn a_restore_that_fails_exits_nonzero_and_says_so() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    std::fs::write(
        fixture.store.join(".newgit/resources/seed.toml"),
        r#"ownership = "branch"

[restore]
mode = "command"
command = "exit 1"
"#,
    )
    .expect("resource");
    checkpointed(&fixture);

    let spawned = spawn_review(&fixture);
    let all = format!("{}\nstderr:\n{}", spawned.stdout, spawned.stderr);
    assert_eq!(spawned.status.code(), Some(1), "{all}");
    // The verdict is on the resource's own line, as `undo` prints it.
    assert!(
        spawned.stdout.contains("resource:  seed command FAILED"),
        "{all}"
    );
    // The headline does not read as a clean spawn.
    assert!(spawned.stdout.contains("`seed` did not come up"), "{all}");
    assert!(
        spawned.stdout.contains("`review` exists but is not ready"),
        "{all}"
    );
    // The recovery record is named, as `undo` names it.
    assert!(spawned.stderr.contains("recovery record at "), "{all}");
    // Exit 1 reports the state; the instance is kept, as a plain spawn's is.
    assert!(workspace_of(&fixture, "review").is_dir());
}

/// A resource the checkpoint holds no state for keeps whatever the spawn's
/// own bind left it in, and that counts too: the `--json` form already
/// exits 1 for it, and the human-readable form must not say less.
#[test]
fn a_resource_the_checkpoint_never_had_is_reported_from_its_bind() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    checkpointed(&fixture);
    std::fs::write(
        fixture.store.join(".newgit/resources/late.toml"),
        r#"ownership = "branch"

[actions.prepare]
command = "exit 1"
"#,
    )
    .expect("resource");

    let spawned = spawn_review(&fixture);
    let all = format!("{}\nstderr:\n{}", spawned.stdout, spawned.stderr);
    assert_eq!(spawned.status.code(), Some(1), "{all}");
    assert!(
        spawned.stdout.contains("resource:  late prepare: FAILED"),
        "{all}"
    );
    assert!(spawned.stdout.contains("`late` did not come up"), "{all}");
    // Nothing was restored for it, so no restore verdict and no recovery
    // record are claimed.
    assert!(!spawned.stdout.contains("late command"), "{all}");
    assert!(!spawned.stderr.contains("recovery record"), "{all}");
}

/// A restore that succeeds still reads as it did, with its verdict added
/// and a clean exit.
#[test]
fn a_restore_that_succeeds_exits_zero_with_its_verdict() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    std::fs::write(
        fixture.store.join(".newgit/resources/seed.toml"),
        r#"ownership = "branch"

[restore]
mode = "command"
command = "true"
"#,
    )
    .expect("resource");
    checkpointed(&fixture);

    let spawned = spawn_review(&fixture);
    let all = format!("{}\nstderr:\n{}", spawned.stdout, spawned.stderr);
    assert_eq!(spawned.status.code(), Some(0), "{all}");
    assert!(
        spawned
            .stdout
            .contains("Spawned `review` from feature-a/ckpt_001, taken by "),
        "{all}"
    );
    assert!(
        spawned.stdout.contains("resource:  seed command ok"),
        "{all}"
    );
    assert!(!spawned.stdout.contains("did not come up"), "{all}");
    assert!(!spawned.stderr.contains("recovery record"), "{all}");
}

/// A checkpoint that holds state for a resource whose restore runs nothing
/// (no `[restore]`) leaves it as the spawn's bind left it. When that bind
/// failed, the restore's `ok` must not stand alone on the line: the
/// resource is not up, and the line says why and where the log is.
#[test]
fn a_restore_that_runs_nothing_still_reports_a_failed_bind() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let flag = temp.join("break-prepare");
    std::fs::write(
        fixture.store.join(".newgit/resources/late.toml"),
        format!(
            r#"ownership = "branch"

[actions.prepare]
command = "test ! -e {flag}"
"#
        ),
    )
    .expect("resource");
    checkpointed(&fixture);
    // The checkpoint holds `late`; from here on its prepare fails.
    std::fs::write(&flag, "").expect("flag");

    let spawned = spawn_review(&fixture);
    let all = format!("{}\nstderr:\n{}", spawned.stdout, spawned.stderr);
    assert_eq!(spawned.status.code(), Some(1), "{all}");
    assert!(
        spawned
            .stdout
            .contains("resource:  late none ok; prepare: FAILED (log: "),
        "{all}"
    );
    assert!(
        spawned
            .stdout
            .contains("re-run with `newgit action late.prepare`"),
        "{all}"
    );
    assert!(spawned.stdout.contains("`late` did not come up"), "{all}");
    // The restore itself did not fail, so no recovery record is claimed.
    assert!(!spawned.stderr.contains("recovery record"), "{all}");
}

/// The human-readable form judges by the same condition `--json` does, so
/// the same fixture exits the same way under both.
#[test]
fn the_human_and_json_forms_exit_alike() {
    for (restore, code) in [("exit 1", 1), ("true", 0)] {
        let (_guard, temp) = tempdir();
        let fixture = setup(&temp);
        std::fs::write(
            fixture.store.join(".newgit/resources/seed.toml"),
            format!(
                r#"ownership = "branch"

[restore]
mode = "command"
command = "{restore}"
"#
            ),
        )
        .expect("resource");
        checkpointed(&fixture);

        let human = spawn_review(&fixture);
        let json = fixture.newgit(
            &fixture.store,
            &[
                "spawn",
                "review-json",
                "--from-checkpoint",
                "feature-a/ckpt_001",
                "--json",
            ],
        );
        assert_eq!(human.status.code(), Some(code), "{}", human.stdout);
        assert_eq!(
            json.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&json.stdout)
        );
    }
}
