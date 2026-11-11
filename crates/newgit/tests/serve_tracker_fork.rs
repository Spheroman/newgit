// `newgit tracker create staging-db --from local-db` on `newgit serve` (#415):
// a fork of a tracker outside Git starts, on its first sync, at its upstream's
// `main`. With the same audience it is a new namespace in its upstream's
// store, refs alone, so nothing is copied; with a narrower one it is made in
// that audience's store, which takes only what the upstream's `main` reaches.

include!("support/serve.rs");

fn project(temp: &Utf8Path, home: &Utf8Path, server: &Server) -> Utf8PathBuf {
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "members only\n").expect("write");
    git(home, &store, &["add", "."]);
    git(home, &store, &["commit", "-q", "-m", "initial"]);
    newgit(home, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    git(home, &store, &["add", "-A"]);
    git(home, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(home, &store, &["remote", "add", "--url", &server.url]);
    git(home, &store, &["commit", "-q", "-am", "link"]);
    newgit(
        home,
        &store,
        &["tracker", "create", "local-db", "--storage", "remote"],
    );
    newgit(home, &store, &["tracker", "track", "local-db", ".env.db"]);
    git(home, &store, &["add", "-A"]);
    git(home, &store, &["commit", "-q", "-m", "local-db"]);
    newgit(home, &store, &["merge", "main"]);
    store
}

fn workspace(home: &Utf8Path, store: &Utf8Path, instance: &str) -> Utf8PathBuf {
    Utf8PathBuf::from(newgit(home, store, &["status", instance, "--path"]))
}

fn main_in(home: &Utf8Path, store: &Utf8Path, namespace: &str) -> Option<String> {
    let output = run(
        "git",
        home,
        store,
        &[
            "--git-dir",
            store.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/namespaces/t/refs/namespaces/{namespace}/refs/heads/main"),
        ],
    );
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[test]
fn a_fork_starts_at_its_upstream_in_the_store_its_audience_calls_for() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    login(&server, &home, "jack");
    let eve = login(&server, &temp.join("eve"), "eve");
    let store = project(&temp, &home, &server);
    let stores = server.data.join("homes/jack/store/stores");
    let devs = stores.join("project-devs.git");

    // `local-db`'s `main`, on the server.
    newgit(&home, &store, &["spawn", "a"]);
    let a = workspace(&home, &store, "a");
    std::fs::write(a.join(".env.db"), "DATABASE_URL=postgres://local\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "local-db", "a"]);
    newgit(&home, &store, &["tracker", "merge", "local-db", "a"]);
    let upstream = main_in(&home, &devs, "local-db").expect("local-db's main on the server");

    // Two forks: one for the members, one for jack alone.
    newgit(
        &home,
        &store,
        &[
            "tracker",
            "create",
            "staging-db",
            "--role",
            "db",
            "--from",
            "local-db",
        ],
    );
    newgit(
        &home,
        &store,
        &[
            "tracker",
            "create",
            "jack-db",
            "--from",
            "local-db",
            "--audience",
            "user:jack",
        ],
    );
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "forks of local-db"]);
    newgit(&home, &store, &["merge", "main"]);

    for (instance, fork) in [("b", "staging-db"), ("c", "jack-db")] {
        newgit(&home, &store, &["spawn", instance, "--use", fork]);
        newgit(&home, &store, &["checkpoint", instance]);
    }

    // The members' fork is a namespace beside its upstream, at its commit.
    assert_eq!(main_in(&home, &devs, "staging-db"), Some(upstream.clone()));
    // Jack's is in jack's store, which holds that history now, as a fork of
    // jack's alone: no other tracker's namespace came with it.
    let jack = stores.join("user/jack.git");
    assert_eq!(main_in(&home, &jack, "jack-db"), Some(upstream.clone()));
    assert_eq!(main_in(&home, &jack, "local-db"), None);
    assert_eq!(main_in(&home, &devs, "jack-db"), None);

    // And it is served as jack's: someone else reads neither fork.
    let reader = temp.join("eve-repo");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    for fork in ["staging-db", "jack-db"] {
        let url = format!("{}/_git/jack.store.t.{fork}.git", server.url);
        assert!(
            !git_as(&eve, &reader, &["ls-remote", &url]).status.success(),
            "eve read {fork}"
        );
    }
}
