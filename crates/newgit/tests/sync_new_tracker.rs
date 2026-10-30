//! A secret tracker the remote does not know yet: its definition reaches the
//! default branch only through the very push whose checkpoint would sync it.
//! The checkpoint syncs everything else and leaves that tracker for later.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn a_tracker_the_remote_refuses_is_left_out_and_synced_once_it_is_defined() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "site",
            "--audience",
            "project-devs",
            "--storage",
            "remote",
        ],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "site", "site"]);
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .undefined_trackers
        .insert("site".to_owned());

    fixture.ok(&fixture.store, &["spawn", "landing"]);
    let workspace = workspace_of(&fixture, "landing");
    std::fs::create_dir_all(workspace.join("site")).expect("mkdir");
    std::fs::write(workspace.join("site/index.html"), "<h1>hi</h1>\n").expect("write");
    std::fs::write(workspace.join("wip.txt"), "uncommitted\n").expect("write");

    // The checkpoint reaches the remote without `site`, and says so.
    let output = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(output.status.success());
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("synced to jack/store, without `site`"),
        "{stdout}"
    );
    let output = stderr;
    assert!(
        output.contains("`site` is not in checkpoint ckpt_001 on the remote")
            && output.contains("is not defined on"),
        "{output}"
    );
    let fork = fixture.remote.repo("jack.store.i.landing.root");
    let files = files_at(&fork, "refs/newgit/checkpoints/ckpt_001");
    assert!(files.contains("wip.txt"), "{files}");
    assert!(!fixture.remote.repo("jack.store.t.site").exists());
    {
        let state = fixture.remote.state.lock().expect("state");
        let record = &state.checkpoints[&("landing".to_owned(), "ckpt_001".to_owned())];
        // Absent when empty.
        let names: Vec<&str> = record["tracker_states"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|state| state["name"].as_str())
            .collect();
        assert!(
            !names.contains(&"site"),
            "the record names no rev the remote lacks: {record}"
        );
    }

    // Once the definition is on the default branch, the next checkpoint
    // brings the tracker.
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .undefined_trackers
        .clear();
    let output = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("synced to jack/store\n"), "{stdout}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("is not in checkpoint"));
    let lane_refs = git(
        &fixture.remote.repo("jack.store.t.site"),
        &["for-each-ref", "--format=%(refname)", "refs/trackers/"],
    );
    assert!(
        lane_refs.contains("refs/trackers/site/instances/landing"),
        "{lane_refs}"
    );
    let state = fixture.remote.state.lock().expect("state");
    let record = &state.checkpoints[&("landing".to_owned(), "ckpt_002".to_owned())];
    assert!(
        record["tracker_states"]
            .as_array()
            .expect("tracker states")
            .iter()
            .any(|state| state["name"] == "site"),
        "{record}"
    );
}

#[test]
fn a_push_publishes_the_code_that_defines_a_new_tracker() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "site",
            "--audience",
            "project-devs",
            "--storage",
            "remote",
        ],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "site", "site"]);
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .undefined_trackers
        .insert("site".to_owned());

    fixture.ok(&fixture.store, &["spawn", "landing"]);
    let workspace = workspace_of(&fixture, "landing");
    std::fs::create_dir_all(workspace.join("site")).expect("mkdir");
    std::fs::write(workspace.join("site/index.html"), "<h1>hi</h1>\n").expect("write");
    std::fs::write(workspace.join("feature.txt"), "work\n").expect("write");
    git(&workspace, &["add", "feature.txt"]);
    git(&workspace, &["commit", "-q", "-m", "work"]);

    // The push runs newgit from its hooks, as this fixture's user.
    let output = Command::new("git")
        .args(["-C", workspace.as_str(), "push", "origin", "landing"])
        .env("HOME", fixture.temp.join("home").as_str())
        .output()
        .expect("git runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("`site` is not in checkpoint"), "{stderr}");
}

#[test]
fn a_tracker_the_remote_refuses_fails_the_checkpoint_sync() {
    // Only an absence is left for later. A real "no" (out of the audience,
    // removed from the project) would never sync, so it must not be passed
    // off as one that will.
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "site",
            "--audience",
            "project-devs",
            "--storage",
            "remote",
        ],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "site", "site"]);
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .denied_trackers
        .insert("site".to_owned());

    fixture.ok(&fixture.store, &["spawn", "landing"]);
    let workspace = workspace_of(&fixture, "landing");
    std::fs::create_dir_all(workspace.join("site")).expect("mkdir");
    std::fs::write(workspace.join("site/index.html"), "<h1>hi</h1>\n").expect("write");

    let output = fixture.newgit(&workspace, &["checkpoint"]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !stdout.contains("synced to jack/store"),
        "a refusal is not a sync: {stdout}"
    );
    assert!(stderr.contains("not in `site`'s audience"), "{stderr}");
    assert!(!stderr.contains("is not in checkpoint"), "{stderr}");
    let state = fixture.remote.state.lock().expect("state");
    assert!(
        !state
            .checkpoints
            .contains_key(&("landing".to_owned(), "ckpt_001".to_owned())),
        "nothing went up with the tracker missing"
    );
}
