// `newgit serve`, end to end: the real CLI against the real server, started
// as its own process on a free port. The whole loop an attempt makes — link,
// spawn, checkpoint, spawn someone else's checkpoint, merge, clone, tag — and
// the refusals that make it safe.

include!("support/serve.rs");

#[test]
fn an_attempt_travels_and_lands_through_newgit_serve() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (jack, ana) = (temp.join("jack"), temp.join("ana"));
    let jack_token = login(&server, &jack, "jack");
    let ana_token = login(&server, &ana, "ana");

    // jack links a project.
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&jack, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    git(&jack, &store, &["add", "."]);
    git(&jack, &store, &["commit", "-q", "-m", "initial"]);
    newgit(&jack, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    git(&jack, &store, &["add", "-A"]);
    git(&jack, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(&jack, &store, &["remote", "add", "--url", &server.url]);
    newgit(&jack, &store, &["members", "add", "ana"]);
    let linked = git(&jack, &store, &["rev-parse", "HEAD"]);

    // Plain Git clones it, with the project's own commit ids.
    let endpoint = format!("{}/jack/store.git", server.url);
    let plain = temp.join("plain");
    let cloned = git_as(
        &jack_token,
        &temp,
        &["clone", "-q", &endpoint, plain.as_str()],
    );
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    assert_eq!(git(&jack, &plain, &["rev-parse", "HEAD"]), linked);

    // An instance's work goes up with its checkpoint.
    newgit(&jack, &store, &["spawn", "feature"]);
    let workspace = Utf8PathBuf::from(newgit(&jack, &store, &["status", "feature", "--path"]));
    std::fs::write(workspace.join("README.md"), "hello\nchanged\n").expect("write");
    git(&jack, &workspace, &["commit", "-q", "-am", "agent change"]);
    std::fs::write(workspace.join("notes.txt"), "wip\n").expect("write");
    git(&jack, &workspace, &["push", "-q"]);
    let agent_commit = git(&jack, &workspace, &["rev-parse", "HEAD"]);

    // ana spawns jack's checkpoint: the same commit, and his uncommitted work.
    let ana_clone = temp.join("ana-store");
    newgit(
        &ana,
        &temp,
        &[
            "clone",
            "jack/store",
            ana_clone.as_str(),
            "--url",
            &server.url,
        ],
    );
    let (_, listed) = http(
        &server.url,
        "GET",
        "/v1/projects/jack/store/checkpoints?instance=feature",
        &[("authorization", &format!("Bearer {ana_token}"))],
        "",
    );
    let checkpoint = listed["checkpoints"]
        .as_array()
        .and_then(|all| all.last())
        .and_then(|last| last["id"].as_str())
        .expect("a checkpoint")
        .to_owned();
    newgit(
        &ana,
        &ana_clone,
        &[
            "spawn",
            "--from-checkpoint",
            &format!("feature/{checkpoint}"),
            "review",
        ],
    );
    let review = Utf8PathBuf::from(newgit(&ana, &ana_clone, &["status", "review", "--path"]));
    assert_eq!(git(&ana, &review, &["rev-parse", "HEAD"]), agent_commit);
    assert_eq!(
        std::fs::read_to_string(review.join("notes.txt")).expect("notes"),
        "wip\n"
    );

    // jack merges; plain Git then gets the agent's own commit.
    newgit(&jack, &store, &["merge", "feature"]);
    let after = temp.join("after");
    let cloned = git_as(
        &jack_token,
        &temp,
        &["clone", "-q", &endpoint, after.as_str()],
    );
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    assert_eq!(git(&jack, &after, &["rev-parse", "HEAD"]), agent_commit);
    assert!(
        !after.join("notes.txt").exists(),
        "uncommitted work stays in checkpoints"
    );

    // A tag on `main` is served to readers as a tag.
    git(&jack, &store, &["pull", "-q"]);
    git(&jack, &store, &["tag", "v1"]);
    git(&jack, &store, &["push", "-q", "origin", "v1"]);
    let tags = git_as(&jack_token, &temp, &["ls-remote", "--tags", &endpoint]);
    assert!(
        String::from_utf8_lossy(&tags.stdout).contains("refs/tags/v1"),
        "{}",
        String::from_utf8_lossy(&tags.stderr)
    );

    // ── Refusals ──
    let eve_token = login(&server, &temp.join("eve"), "eve");
    let root_repo = format!("{}/_git/jack.store.t.root.git", server.url);
    let fork = format!("{}/_git/jack.store.i.feature.root.git", server.url);
    let refused = |output: &Output, why: &str| {
        assert!(!output.status.success(), "{why}: it was allowed");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            said.contains("403") || said.contains("not found"),
            "{why}: {said}"
        );
    };
    refused(
        &git_as(&eve_token, &temp, &["ls-remote", &root_repo]),
        "a non-member reads a tracker",
    );
    refused(
        &git_as(&eve_token, &temp, &["ls-remote", &endpoint]),
        "a non-member clones",
    );
    refused(
        &git_as(&ana_token, &plain, &["push", &fork, "HEAD:refs/heads/x"]),
        "a member pushes to someone else's instance",
    );
    refused(
        &git_as(
            &jack_token,
            &plain,
            &["push", &root_repo, "HEAD:refs/heads/x"],
        ),
        "the owner pushes straight to a tracker in Git",
    );
    // A tracker's own readers never see the instances inside it.
    let advertised = git_as(&jack_token, &temp, &["ls-remote", &root_repo]);
    assert!(!String::from_utf8_lossy(&advertised.stdout).contains("namespaces"));
    // A fork is refs inside the tracker's namespace, not a copy.
    assert!(
        !server
            .data
            .join("homes/jack/store/stores/instance")
            .exists()
    );
}
