//! An instance behind `main` has not changed what `main` moved. Every new
//! instance's first push forked t3code, nine minutes, when it had touched
//! only root and t3code's pin had moved on `main` since it spawned (#144).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// A project with `t3code` mounted, published, and an instance spawned from
/// it, then `main` moving `t3code` on without the instance.
fn instance_behind_main(fixture: &Fixture) -> Utf8PathBuf {
    let upstream = fixture.temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "newgit"]);
    std::fs::write(upstream.join("app.ts"), "app\n").expect("write");
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "t3: first"]);
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "t3code",
            "--git",
            "--mount",
            "apps/t3code",
            "--from",
            upstream.as_str(),
        ],
    );
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "feature"]);

    std::fs::write(fixture.store.join("apps/t3code/app.ts"), "moved\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "t3: moved on main"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    workspace_of(fixture, "feature")
}

fn forks(fixture: &Fixture) -> Vec<String> {
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .forks
        .get("feature")
        .cloned()
        .unwrap_or_default()
}

#[test]
fn an_instance_behind_main_does_not_fork_what_main_moved() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = instance_behind_main(&fixture);

    std::fs::write(workspace.join("change.txt"), "work\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "root work"]);
    fixture.ok(&fixture.store, &["checkpoint", "feature"]);

    assert!(forks(&fixture).contains(&"root".to_owned()));
    assert!(
        !forks(&fixture).contains(&"t3code".to_owned()),
        "it never touched t3code: {:?}",
        forks(&fixture)
    );
}

#[test]
fn merging_an_instance_behind_main_keeps_what_main_moved() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = instance_behind_main(&fixture);

    std::fs::write(workspace.join("change.txt"), "work\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "root work"]);
    fixture.ok(&fixture.store, &["checkpoint", "feature"]);
    assert!(!forks(&fixture).contains(&"t3code".to_owned()));

    fixture.ok(&fixture.store, &["merge", "feature", "--into", "main"]);

    assert_eq!(
        git(&fixture.store, &["show", "main:apps/t3code/app.ts"]),
        "moved",
        "main's t3code did not go back"
    );
    assert_eq!(
        git(&fixture.store, &["show", "main:change.txt"]),
        "work",
        "and the instance's work landed"
    );
}

#[test]
fn an_instance_that_changed_a_tracker_still_forks_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = instance_behind_main(&fixture);

    std::fs::write(workspace.join("apps/t3code/own.ts"), "own\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "t3 work"]);
    fixture.ok(&fixture.store, &["checkpoint", "feature"]);

    assert!(
        forks(&fixture).contains(&"t3code".to_owned()),
        "it has work there to send: {:?}",
        forks(&fixture)
    );
}

#[test]
fn uncommitted_work_in_a_tracker_still_forks_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = instance_behind_main(&fixture);

    std::fs::write(workspace.join("apps/t3code/app.ts"), "uncommitted\n").expect("write");
    fixture.ok(&fixture.store, &["checkpoint", "feature"]);

    assert!(
        forks(&fixture).contains(&"t3code".to_owned()),
        "the checkpoint has work there to send: {:?}",
        forks(&fixture)
    );
}
