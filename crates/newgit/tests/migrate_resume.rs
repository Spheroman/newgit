//! `newgit tracker migrate` stopped part way through its remote half — a 502
//! when the merge finishes, a checkpoint write that fails, the local ref
//! transaction failing after the remote moved — and run again: it resumes
//! with the same commits and ends where an uninterrupted run does. Also: a
//! checkpoint this login may not rewrite is said and skipped, every branch's
//! list migrates, and a branch that cannot is refused before anything is
//! pushed.

#![allow(dead_code)]

include!("support/fake_remote.rs");
include!("support/old_layout.rs");

fn migrate(fixture: &Fixture) -> Output {
    fixture.newgit(&fixture.store, &["tracker", "migrate"])
}

fn root_main_on_remote(fixture: &Fixture) -> String {
    git(
        &fixture.remote.repo("jack.store.t.root"),
        &["rev-parse", "main"],
    )
}

#[test]
fn a_502_when_the_merge_finishes_resumes_to_the_same_end() {
    let (_guard, temp) = tempdir();
    let old = old_project(&temp);
    let fixture = &old.fixture;
    fixture.remote.state.lock().expect("state").fail_finish_once = true;
    let failed = migrate(fixture);
    assert!(!failed.status.success(), "the 502 stops it");
    // Root's `main` moved, the list did not: the half-moved remote.
    assert_ne!(root_main_on_remote(fixture), old.main);
    assert_eq!(
        fixture.remote.state.lock().expect("state").branches["jack/store"]["main"]["project"],
        json!(old.main)
    );
    let saved = git(
        &fixture.store,
        &["rev-parse", "refs/newgit-migrating/carve"],
    );

    let resumed = fixture.ok(&fixture.store, &["tracker", "migrate"]);
    assert!(resumed.contains("Resuming"), "{resumed}");
    assert_eq!(
        git(&fixture.store, &["rev-parse", "HEAD"]),
        saved,
        "the same migration commit, not a new one"
    );
    assert_migrated(&old);
}

#[test]
fn a_checkpoint_write_that_fails_resumes_to_the_same_end() {
    let (_guard, temp) = tempdir();
    let old = old_project(&temp);
    let fixture = &old.fixture;
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .fail_checkpoint_writes = 1;
    assert!(!migrate(fixture).status.success());
    // The list is new already: a rerun cannot merge again, and need not.
    let list = fixture.remote.state.lock().expect("state").branches["jack/store"]["main"].clone();
    assert!(list.get("project").is_none(), "{list}");

    fixture.ok(&fixture.store, &["tracker", "migrate"]);
    assert_migrated(&old);
}

#[test]
fn a_local_failure_after_the_remote_moved_resumes_to_the_same_end() {
    let (_guard, temp) = tempdir();
    let old = old_project(&temp);
    let fixture = &old.fixture;
    // Something holds the store's ref lock: the local transaction fails,
    // after the remote has moved.
    let lock = fixture
        .store
        .join(".newgit/local/trackers.git/refs/trackers/root/main.lock");
    std::fs::write(&lock, "").expect("lock");
    let failed = migrate(fixture);
    assert!(!failed.status.success());
    let list = fixture.remote.state.lock().expect("state").branches["jack/store"]["main"].clone();
    assert!(list.get("project").is_none(), "the remote moved: {list}");
    std::fs::remove_file(&lock).expect("unlock");

    let resumed = fixture.ok(&fixture.store, &["tracker", "migrate"]);
    assert!(resumed.contains("Resuming"), "{resumed}");
    assert_migrated(&old);
}

#[test]
fn a_checkpoint_this_login_may_not_rewrite_is_said_and_skipped() {
    let (_guard, temp) = tempdir();
    let old = old_project(&temp);
    let fixture = &old.fixture;
    {
        let mut state = fixture.remote.state.lock().expect("state");
        let key = ("feature".to_owned(), "ckpt_001".to_owned());
        let mut theirs = state.checkpoints[&key].clone();
        theirs["branch"] = json!("theirs");
        state
            .checkpoints
            .insert(("theirs".to_owned(), "ckpt_001".to_owned()), theirs);
        state.foreign_instances.insert("theirs".to_owned());
    }
    let out = fixture.ok(&fixture.store, &["tracker", "migrate"]);
    assert!(out.contains("may not rewrite"), "{out}");
    assert!(out.contains("theirs/ckpt_001"), "{out}");
    let theirs = fixture.remote.state.lock().expect("state").checkpoints
        [&("theirs".to_owned(), "ckpt_001".to_owned())]
        .clone();
    assert!(
        theirs["code"]["head"].get("project").is_some(),
        "left as it was"
    );
    assert_migrated(&old);
}

#[test]
fn every_branch_list_migrates_and_one_that_cannot_is_refused_before_anything_moves() {
    let (_guard, temp) = tempdir();
    let old = old_project(&temp);
    let fixture = &old.fixture;
    // `release`, as the old engine listed the instance's state: a root
    // tracker commit split from the instance's view, noted on it.
    let tree = git(
        &fixture.store,
        &["rev-parse", &format!("{}^{{tree}}", old.feature)],
    );
    let old_root = git(
        &fixture.store,
        &[
            "commit-tree",
            &tree,
            "-p",
            &old.main,
            "-m",
            "feature work (root's half)",
        ],
    );
    let note = format!(
        "access {}\nproject {}\nroot {old_root}\n",
        old.main, old.main
    );
    git(
        &fixture.store,
        &[
            "notes",
            "--ref",
            "refs/notes/newgit-composed",
            "add",
            "-f",
            "-m",
            &note,
            &old.feature,
        ],
    );
    let release = json!({ "root": old_root, "project": old.main, "access": old.main });
    let stray = json!({ "root": "f".repeat(40), "project": old.main, "access": old.main });
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .branches
        .get_mut("jack/store")
        .expect("project")
        .insert("stray".to_owned(), stray.clone());
    let before = fixture.remote.requests().len();
    let refused = migrate(fixture);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("`stray`"), "{stderr}");
    assert!(stderr.contains("nothing was pushed"), "{stderr}");
    assert!(
        fixture.remote.requests()[before..]
            .iter()
            .all(|(method, _, _)| method == "GET"),
        "nothing written: {:?}",
        &fixture.remote.requests()[before..]
    );
    assert_eq!(root_main_on_remote(fixture), old.main);

    {
        let mut state = fixture.remote.state.lock().expect("state");
        let branches = state.branches.get_mut("jack/store").expect("project");
        branches.remove("stray");
        branches.insert("release".to_owned(), release);
    }
    fixture.ok(&fixture.store, &["tracker", "migrate"]);
    let list =
        fixture.remote.state.lock().expect("state").branches["jack/store"]["release"].clone();
    assert_eq!(list, json!({ "root": old.feature }));
    assert_eq!(
        git(
            &fixture.remote.repo("jack.store.t.root"),
            &["rev-parse", "release"]
        ),
        old.feature
    );
    assert_migrated(&old);
}
