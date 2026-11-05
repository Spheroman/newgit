//! A merge that stops after root's ref moved, before the finish (a 502, or a
//! timeout while the remote runs Git over a large history), finishes on the
//! next run. Root's ref already holds this merge's own commit, so the next
//! run finishes it rather than taking it for another merge landing, which
//! left the project unable to merge at all (found bringing t3code in, #144).
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
    let pushed = root_main_on_remote(&fixture);
    assert_ne!(pushed, before, "root's ref moved");
    assert_eq!(listed_root(&fixture), before, "the list did not");

    fixture.ok(&fixture.store, &["merge", "main"]);
    assert_eq!(listed_root(&fixture), pushed, "the same commit, now listed");
    assert_eq!(root_main_on_remote(&fixture), pushed);
}

#[test]
fn another_commit_on_roots_ref_is_still_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);

    // Someone else's commit on root's ref that no list names.
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
    let output = fixture.newgit(&fixture.store, &["merge", "main"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("another merge is landing"), "{stderr}");
}
