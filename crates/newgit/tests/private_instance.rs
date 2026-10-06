//! `newgit spawn --private`: an instance only its owner sees on the remote
//! until its work is merged — and none at all without a newgit remote, where
//! nothing could keep it private.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn a_private_spawn_registers_the_instance_private_and_says_so() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let stdout = fixture.ok(&fixture.store, &["spawn", "fix-cve", "--private"]);
    assert!(
        stdout.contains("only you see this instance on jack/store"),
        "{stdout}"
    );
    fixture.ok(&fixture.store, &["spawn", "docs"]);
    let status = fixture.ok(&fixture.store, &["status", "fix-cve"]);
    assert!(status.contains("ready, private"), "{status}");
    let state = fixture.remote.state.lock().expect("state");
    assert!(state.private.contains("fix-cve"));
    assert!(!state.private.contains("docs"));
}

#[test]
fn a_private_spawn_that_could_not_register_registers_private_when_it_can() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    // Point the project at a remote that is not there: the spawn succeeds,
    // and registration is retried at the next checkpoint.
    let config = fixture.store.join(".newgit/config.toml");
    let text = std::fs::read_to_string(&config).expect("config");
    let unreachable = text.replace(&fixture.remote.url, "http://127.0.0.1:9");
    std::fs::write(&config, &unreachable).expect("write");
    let stdout = fixture.ok(&fixture.store, &["spawn", "fix-cve", "--private"]);
    assert!(stdout.contains("not on the remote yet"), "{stdout}");

    std::fs::write(&config, &text).expect("write");
    fixture.ok(&fixture.store, &["checkpoint", "fix-cve"]);
    assert!(
        fixture
            .remote
            .state
            .lock()
            .expect("state")
            .private
            .contains("fix-cve")
    );
}

#[test]
fn a_private_spawn_without_a_newgit_remote_is_refused() {
    let (_guard, temp) = tempdir();
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    git(&store, &["add", "."]);
    git(&store, &["commit", "-q", "-m", "initial"]);
    let fixture = Fixture {
        temp: temp.clone(),
        store: store.clone(),
        remote: FakeRemote::start(temp.join("artifacts")),
    };
    fixture.ok(&store, &["init"]);
    let output = fixture.newgit(&store, &["spawn", "fix-cve", "--private"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("newgit remote add"), "{stderr}");
}

#[test]
fn a_remote_that_ignores_private_never_receives_the_instances_work() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.remote.state.lock().expect("state").ignores_private = true;
    let stdout = fixture.ok(&fixture.store, &["spawn", "fix-cve", "--private"]);
    assert!(stdout.contains("ordinary instance"), "{stdout}");

    let workspace = workspace_of(&fixture, "fix-cve");
    std::fs::write(workspace.join("README.md"), "the fix\n").expect("write");
    git(&workspace, &["commit", "-qam", "the fix"]);
    fixture.newgit(&fixture.store, &["checkpoint", "fix-cve"]);
    let state = fixture.remote.state.lock().expect("state");
    assert!(state.forks.get("fix-cve").is_none_or(Vec::is_empty));
    assert!(
        !state
            .checkpoints
            .keys()
            .any(|(instance, _)| instance == "fix-cve")
    );
}
