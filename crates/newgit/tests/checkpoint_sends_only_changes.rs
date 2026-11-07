//! A checkpoint on a linked project sends what changed, and only that. Every
//! checkpoint used to ask for a credential and push each tracker it syncs,
//! changed or not, and ask the remote again for a fork it had already made:
//! about a second each against newgit.dev, most of a warm checkpoint.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn since(fixture: &Fixture, before: usize, what: &str) -> usize {
    fixture.remote.requests()[before..]
        .iter()
        .filter(|(method, path, _)| method == "POST" && path.ends_with(what))
        .count()
}

#[test]
fn a_checkpoint_sends_only_the_trackers_that_changed() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "runtime-env",
            "--audience",
            "user",
            "--storage",
            "remote",
        ],
    );
    fixture.ok(
        &fixture.store,
        &["tracker", "track", "runtime-env", ".env.local"],
    );
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join(".env.local"), "KEY=one\n").expect("write");
    std::fs::write(workspace.join("wip.txt"), "one\n").expect("write");

    let before = fixture.remote.requests().len();
    fixture.ok(&workspace, &["checkpoint"]);
    let first = since(&fixture, before, "/credentials");
    assert!(first >= 2, "root's fork and runtime-env at least: {first}");
    assert_eq!(
        since(&fixture, before, "/fork"),
        1,
        "root's fork, made once"
    );

    // Only root's files changed: runtime-env is not sent again, and the fork
    // is not asked for again.
    std::fs::write(workspace.join("wip.txt"), "two\n").expect("write");
    let before = fixture.remote.requests().len();
    fixture.ok(&workspace, &["checkpoint"]);
    assert_eq!(
        since(&fixture, before, "/credentials"),
        1,
        "root's fork alone: runtime-env unchanged needs no credential and no push"
    );
    assert_eq!(
        since(&fixture, before, "/fork"),
        0,
        "the fork is remembered"
    );

    // Changed again: sent again, and the remote has it.
    std::fs::write(workspace.join(".env.local"), "KEY=two\n").expect("write");
    let before = fixture.remote.requests().len();
    fixture.ok(&workspace, &["checkpoint"]);
    assert_eq!(
        since(&fixture, before, "/credentials"),
        2,
        "root's fork and runtime-env"
    );
    let repo = fixture.remote.repo("jack.store.t.runtime-env.jack");
    let instance = git(
        &repo,
        &["rev-parse", "refs/trackers/runtime-env/instances/feature-a"],
    );
    let local = git(
        &fixture.store.join(".newgit/local/trackers.git"),
        &["rev-parse", "refs/trackers/runtime-env/instances/feature-a"],
    );
    assert_eq!(instance, local, "the remote has the latest");
}
