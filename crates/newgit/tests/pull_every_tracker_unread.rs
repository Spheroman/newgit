//! `newgit pull` with no arguments (#345) when the remote refuses reads
//! (403): refused for every tracker, it fails instead of reporting that
//! nothing is behind; refused for some with the rest up to date, it says
//! the readable ones are up to date and names the refused ones.
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

fn deny(fixture: &Fixture, tracker: &str) {
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .denied_trackers
        .insert(tracker.to_owned());
}

#[test]
fn every_tracker_refused_fails_instead_of_saying_nothing_is_behind() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    two_trackers(&fixture);
    advance_on_remote(&fixture, "lib");
    let before = git(&fixture.store, &["rev-parse", "HEAD"]);
    // The caller refused everywhere (a token for another project): every
    // mounted tracker, `project` and `access` among them.
    for tracker in ["project", "access", "lib", "ui"] {
        deny(&fixture, tracker);
    }

    let output = fixture.newgit(&fixture.store, &["pull"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "should fail:\n{stdout}");
    assert!(!stdout.contains("Nothing to pull"), "{stdout}");
    assert!(
        stderr.contains("no mounted tracker could be read on the remote"),
        "{stderr}"
    );
    assert!(stderr.contains("403"), "{stderr}");
    assert!(stderr.contains("not in `lib`'s audience"), "{stderr}");
    assert!(stderr.contains("not in `ui`'s audience"), "{stderr}");
    assert_eq!(git(&fixture.store, &["rev-parse", "HEAD"]), before);
}

#[test]
fn some_refused_and_the_rest_up_to_date_does_not_claim_nothing_is_behind() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    two_trackers(&fixture);
    advance_on_remote(&fixture, "lib");
    let before = git(&fixture.store, &["rev-parse", "HEAD"]);
    deny(&fixture, "lib");

    let stdout = fixture.ok(&fixture.store, &["pull"]);
    assert!(!stdout.contains("Nothing to pull"), "{stdout}");
    assert!(
        stdout.contains(
            "The trackers you can read are up to date in the project root; 1 tracker could \
             not be read on the remote:"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("`lib` skipped: you can't read it on the remote"),
        "{stdout}"
    );
    assert!(stdout.contains("not in `lib`'s audience"), "{stdout}");
    assert_eq!(git(&fixture.store, &["rev-parse", "HEAD"]), before);
}
