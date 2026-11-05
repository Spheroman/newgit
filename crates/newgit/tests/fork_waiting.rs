//! Artifacts forks a large repository in the background, and newgit.dev
//! answers `202 {"forking": true}` until the fork is there. A checkpoint
//! sync, which forks each tracker an instance changed, waits for it rather
//! than failing (found forking Spheroman/t3code for an instance, #144).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn a_sync_waits_while_the_remote_is_still_forking() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join("change.txt"), "work\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "work"]);

    fixture.remote.state.lock().expect("state").forking = 2;
    fixture.ok(&fixture.store, &["checkpoint", "feature-a"]);
    let state = fixture.remote.state.lock().expect("state");
    assert_eq!(state.forking, 0, "it asked until the fork was there");
    assert!(
        state
            .forks
            .get("feature-a")
            .is_some_and(|forks| !forks.is_empty()),
        "and then forked"
    );
}
