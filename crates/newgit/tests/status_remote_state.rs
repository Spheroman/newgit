//! `newgit status` says what the remote has not written down yet, and
//! webhook pulls waiting, failing, or refused: only when there is any, so
//! "not written yet" never reads as "out of date" (docs/design/anchoring.md,
//! build order step 6). `newgit remote show` says it in full.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn status_says_nothing_of_the_remote_when_all_is_written_down() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let status = fixture.ok(&fixture.store, &["status"]);
    assert!(!status.contains("\nOn jack/store"), "{status}");
    let shown = fixture.ok(&fixture.store, &["remote", "show"]);
    assert!(
        shown.contains("everything landed is written down"),
        "{shown}"
    );
}

#[test]
fn status_says_what_is_pending_and_a_refused_pull() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    fixture.remote.state.lock().expect("state").remote_state = json!({
        "branches": [{
            "name": "main",
            "pending": { "root": 2, "t3code": 1 },
            "rollupFailed": { "at": "2026-11-08T00:00:00Z", "why": "Artifacts is down" }
        }],
        "pulls": [{
            "target": "github", "commit": "a".repeat(40), "queuedAt": "2026-11-08T00:00:00Z",
            "attempts": 2, "lastError": "GitHub said 500"
        }],
        "ended": [
            { "target": "t3code", "commit": "b".repeat(40), "outcome": "landed", "at": "2026-11-07T00:00:00Z" },
            { "target": "t3code", "commit": "c".repeat(40), "outcome": "refused",
              "why": "commit 123 changes `apps/t3code/.repos`", "at": "2026-11-08T00:00:00Z" },
            { "target": "github", "commit": "d".repeat(40), "outcome": "refused", "why": "old", "at": "2026-11-06T00:00:00Z" },
            { "target": "github", "commit": "e".repeat(40), "outcome": "landed", "at": "2026-11-07T00:00:00Z" }
        ]
    });
    let status = fixture.ok(&fixture.store, &["status"]);
    assert!(status.contains("On jack/store:"), "{status}");
    assert!(status.contains("`main`: `root` has 2 changes below it not rolled up yet; its last roll-up failed (Artifacts is down)"), "{status}");
    assert!(
        status.contains("`main`: `t3code` has 1 change below it"),
        "{status}"
    );
    assert!(status.contains("the webhook's pull from `github` at aaaaaaaaaaaa is waiting (2 failed attempts; last: GitHub said 500)"), "{status}");
    assert!(
        status.contains("the webhook's pull from `t3code` at cccccccccccc was refused"),
        "{status}"
    );
    assert!(
        !status.contains("dddddddddddd"),
        "a refusal a later landing cleared is not said: {status}"
    );
}
