//! `newgit status` says a pinned tracker held behind its own `main`: held
//! still never reads as forgotten, and moving the pin promotes it.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn status_says_a_pinned_tracker_held_behind_its_own_main() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    fixture.remote.state.lock().expect("state").remote_state = json!({
        "branches": [{
            "name": "main",
            "pending": {},
            "pinned": [{ "tracker": "env__prod", "pin": "a".repeat(40), "main": "b".repeat(40) }]
        }],
        "pulls": [],
        "ended": []
    });
    let status = fixture.ok(&fixture.store, &["status"]);
    assert!(
        status.contains("`main`: `env/prod` is pinned at aaaaaaaaaaaa; its own `main` is at bbbbbbbbbbbb (moving the pin promotes it)"),
        "{status}"
    );
}
