//! A merge that stops before its finish (a 502, or a timeout while the remote
//! runs Git over a large history) finishes on the next run. Its commits wait
//! at landing refs and nothing else moved, so the next run pushes them again
//! (nothing to send) and lands. Before landing refs, root's ref moved first,
//! and a run that stopped there left the project unable to merge at all
//! (found bringing t3code in, #144).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn root_main_on_remote(fixture: &Fixture) -> String {
    git(
        &fixture.remote.repo("jack.store.t.root"),
        &["rev-parse", "main"],
    )
}

fn listed_root(fixture: &Fixture) -> String {
    fixture.remote.state.lock().expect("state").branches["jack/store"]["main"]["root"]
        .as_str()
        .expect("root is listed")
        .to_owned()
}

#[test]
fn a_merge_whose_finish_failed_finishes_on_the_next_run() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let before = listed_root(&fixture);

    std::fs::write(fixture.store.join("change.txt"), "work\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "work"]);

    fixture.remote.state.lock().expect("state").fail_finish_once = true;
    let failed = fixture.newgit(&fixture.store, &["merge", "main"]);
    assert!(!failed.status.success(), "the 502 stops it");
    assert_eq!(
        root_main_on_remote(&fixture),
        before,
        "root's ref did not move"
    );
    assert_eq!(listed_root(&fixture), before, "nor did the list");
    let root = fixture.remote.repo("jack.store.t.root");
    let waiting = git(
        &root,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/newgit/landing/",
        ],
    );
    assert!(!waiting.is_empty(), "the commit waits at its landing ref");

    fixture.ok(&fixture.store, &["merge", "main"]);
    assert_eq!(
        listed_root(&fixture),
        waiting,
        "the same commit, now listed"
    );
    assert_eq!(
        root_main_on_remote(&fixture),
        waiting,
        "and root's ref followed"
    );
    assert_eq!(
        git(&root, &["for-each-ref", "refs/newgit/landing/"]),
        "",
        "its landing ref is gone"
    );
}

#[test]
fn another_commit_on_roots_ref_does_not_stop_a_merge() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);

    // Someone else's commit on root's ref that no list names: the list is
    // the truth, and root's ref follows it.
    let root = fixture.remote.repo("jack.store.t.root");
    let tree = git(&root, &["rev-parse", "main^{tree}"]);
    let parent = git(&root, &["rev-parse", "main"]);
    let other = git(
        &root,
        &["commit-tree", &tree, "-p", &parent, "-m", "someone else"],
    );
    git(&root, &["update-ref", "refs/heads/main", &other]);

    std::fs::write(fixture.store.join("change.txt"), "work\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "work"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    assert_ne!(listed_root(&fixture), parent, "the merge landed");
    assert_eq!(root_main_on_remote(&fixture), listed_root(&fixture));
}
