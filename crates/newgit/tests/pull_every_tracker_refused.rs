//! `newgit pull` with no arguments (#345) on a project linked to a newgit
//! remote that refuses to let the caller read one tracker (403): that tracker
//! is named as skipped and the rest still move, while `newgit pull <tracker>`
//! for it fails loudly.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// `lib` and `ui` carved from the store and published to the remote.
fn two_trackers(fixture: &Fixture) {
    let store = &fixture.store;
    for tracker in ["lib", "ui"] {
        std::fs::create_dir_all(store.join(format!("libs/{tracker}"))).expect("mkdir");
        std::fs::write(store.join(format!("libs/{tracker}/index.js")), "one\n").expect("write");
    }
    git(store, &["add", "-A"]);
    git(store, &["commit", "-q", "-m", "libs"]);
    for tracker in ["lib", "ui"] {
        fixture.ok(
            store,
            &[
                "tracker",
                "create",
                tracker,
                "--git",
                "--path",
                &format!("libs/{tracker}"),
            ],
        );
    }
    fixture.ok(store, &["merge", "main"]);
}

/// Elsewhere: one commit on `tracker`'s `main` on the remote.
fn advance_on_remote(fixture: &Fixture, tracker: &str) -> String {
    let repo = fixture.remote.repo(&format!("jack.store.t.{tracker}"));
    let clone = fixture.temp.join(format!("elsewhere-{tracker}"));
    git(
        &fixture.temp,
        &["clone", "-q", "-b", "main", repo.as_str(), clone.as_str()],
    );
    std::fs::write(clone.join("new.js"), "x\n").expect("write");
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", &format!("{tracker}: new")]);
    git(&clone, &["push", "-q", "origin", "main"]);
    git(&clone, &["rev-parse", "HEAD"])
}

#[test]
fn a_tracker_the_remote_will_not_let_you_read_is_skipped_and_the_rest_move() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    two_trackers(&fixture);
    advance_on_remote(&fixture, "lib");
    advance_on_remote(&fixture, "ui");
    let before = git(&fixture.store, &["rev-parse", "HEAD"]);
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .denied_trackers
        .insert("lib".to_owned());

    let stdout = fixture.ok(&fixture.store, &["pull"]);
    assert!(
        stdout.contains("Updated 1 tracker in the project root, one commit"),
        "{stdout}"
    );
    assert!(
        stdout.contains("ui           1 commit from main"),
        "{stdout}"
    );
    assert!(
        stdout.contains("`lib` skipped: you can't read it on the remote"),
        "{stdout}"
    );
    assert!(stdout.contains("not in `lib`'s audience"), "{stdout}");
    assert_eq!(
        git(&fixture.store, &["log", "-1", "--format=%s"]),
        "Update trackers: ui (1 commit)"
    );
    assert_eq!(git(&fixture.store, &["rev-parse", "HEAD^"]), before);
    assert_eq!(
        std::fs::read_to_string(fixture.store.join("libs/ui/new.js")).expect("ui moved"),
        "x\n"
    );
    assert!(
        !fixture.store.join("libs/lib/new.js").exists(),
        "lib's pin did not move"
    );

    let output = fixture.newgit(&fixture.store, &["pull", "lib"]);
    assert!(
        !output.status.success(),
        "`newgit pull lib` should fail:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("403"), "{stderr}");
    assert!(stderr.contains("not in `lib`'s audience"), "{stderr}");
}
