//! `newgit spawn --from-checkpoint` builds an instance in steps — a spawn,
//! then importing the checkpoint and undoing to it — and holds the new
//! instance's lock across all of them (#212): a command already holding the
//! name makes it wait, and the restore hooks of its undo run under the lock.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn spawn_from_checkpoint_holds_the_new_instance_lock_throughout() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let seen = temp.join("seen");
    std::fs::write(
        fixture.store.join(".newgit/resources/seed.toml"),
        format!(
            r#"ownership = "branch"

[restore]
mode = "command"
command = "echo \"$NEWGIT_LOCKED\" > {seen}"
"#
        ),
    )
    .expect("resource");
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    fixture.ok(&workspace, &["checkpoint"]);

    let store = newgit_core::MetadataStore::at(fixture.store.clone());
    let held = store.lock_instance("review", "review").expect("lock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(fixture.store.as_str())
        .args(["spawn", "review", "--from-checkpoint", "feature-a/ckpt_001"])
        .env_remove("NEWGIT_LOCKED")
        .env("HOME", fixture.temp.join("home").as_str())
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("newgit runs");
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        child.try_wait().expect("wait").is_none(),
        "spawn --from-checkpoint did not wait for the lock: {:?}",
        child.wait_with_output()
    );
    assert!(
        !fixture.store.join(".newgit/branches/review.toml").exists(),
        "the instance was spawned while another command held its lock"
    );
    drop(held);

    let output = child.wait_with_output().expect("output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{}{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("waiting for `review`"), "{stderr}");

    // The undo's restore ran under the same lock, and was told so.
    let locked = std::fs::read_to_string(&seen).expect("restore ran");
    assert!(
        locked
            .trim()
            .split(',')
            .any(|entry| entry.starts_with("review:")),
        "{locked}"
    );
}
