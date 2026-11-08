//! A merge sends its commits to the remote before it begins, so the remote
//! pushes them itself and `begin` hands out no write password; a remote
//! that will not take them gets them pushed with one, as before.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn work(fixture: &Fixture, text: &str) {
    std::fs::write(fixture.store.join("change.txt"), text).expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "work"]);
}

fn listed_root(fixture: &Fixture) -> String {
    fixture.remote.state.lock().expect("state").branches["jack/store"]["main"]["root"]
        .as_str()
        .expect("root is listed")
        .to_owned()
}

#[test]
fn a_merge_sends_its_commits_before_it_begins() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    work(&fixture, "one\n");
    fixture.ok(&fixture.store, &["merge", "main"]);
    let root = listed_root(&fixture);
    let uploads = fixture.remote.state.lock().expect("state").uploads.clone();
    assert_eq!(
        uploads,
        vec![("root".to_owned(), root.clone())],
        "root's commit, sent first"
    );
    assert_eq!(
        git(
            &fixture.remote.repo("jack.store.t.root"),
            &["rev-parse", "main"]
        ),
        root,
        "and root's branch followed it"
    );
}

#[test]
fn a_remote_that_will_not_take_them_gets_them_pushed() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.remote.state.lock().expect("state").refuse_uploads = true;
    work(&fixture, "two\n");
    fixture.ok(&fixture.store, &["merge", "main"]);
    assert!(
        fixture
            .remote
            .state
            .lock()
            .expect("state")
            .uploads
            .is_empty()
    );
    assert_eq!(
        git(
            &fixture.remote.repo("jack.store.t.root"),
            &["rev-parse", "main"]
        ),
        listed_root(&fixture)
    );
}
