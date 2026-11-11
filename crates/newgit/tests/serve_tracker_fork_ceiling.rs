// A fork on `newgit serve` stays held to the upstream it was made from (#415
// review): the server records it when it makes the fork's repo, no push can
// move that record, and a definition that drops `from` and says `public` —
// landed past the client — moves nothing and serves nothing wider. A member
// outside a narrower fork's audience reads none of it.

include!("support/serve.rs");
include!("support/serve_keys.rs");

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

fn ref_in(home: &Utf8Path, store: &Utf8Path, namespace: &str, reference: &str) -> Option<String> {
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
            &format!("refs/namespaces/t/refs/namespaces/{namespace}/{reference}"),
        ],
    );
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn edit(file: &Utf8Path, edit: impl Fn(String) -> String) {
    let text = std::fs::read_to_string(file).expect("read");
    std::fs::write(file, edit(text)).expect("write");
}

#[test]
fn a_fork_stays_held_to_the_upstream_it_was_made_from() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let jack = login(&server, &home, "jack");
    let bob = login(&server, &temp.join("bob"), "bob");
    let eve = login(&server, &temp.join("eve"), "eve");
    publish_keys(&[("jack", &home), ("bob", &temp.join("bob"))]);
    let store = project(&temp, &home, &server);
    newgit(&home, &store, &["members", "add", "bob"]);
    let stores = server.data.join("homes/jack/store/stores");
    let devs = stores.join("project-devs.git");
    let public = stores.join("public.git");

    newgit(&home, &store, &["spawn", "a"]);
    let a = workspace(&home, &store, "a");
    std::fs::write(a.join(".env.db"), "DATABASE_URL=postgres://secret\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "local-db", "a"]);
    newgit(&home, &store, &["tracker", "merge", "local-db", "a"]);
    let upstream = main_in(&home, &devs, "local-db").expect("local-db's main on the server");

    for args in [
        &["--role", "db", "--from", "local-db"][..],
        &["--from", "local-db", "--audience", "user:jack"][..],
    ] {
        let name = if args.contains(&"user:jack") {
            "jack-db"
        } else {
            "staging-db"
        };
        let mut all = vec!["tracker", "create", name];
        all.extend_from_slice(args);
        newgit(&home, &store, &all);
    }
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "forks of local-db"]);
    newgit(&home, &store, &["merge", "main"]);
    for (instance, fork) in [("b", "staging-db"), ("c", "jack-db")] {
        newgit(&home, &store, &["spawn", instance, "--use", fork]);
        newgit(&home, &store, &["checkpoint", instance]);
    }
    assert_eq!(main_in(&home, &devs, "staging-db"), Some(upstream.clone()));

    // The server recorded what each fork was made from.
    let record = "refs/newgit/from/local-db";
    assert!(ref_in(&home, &devs, "staging-db", record).is_some());
    assert!(ref_in(&home, &stores.join("user/jack.git"), "jack-db", record).is_some());

    // A member who isn't jack reads the members' fork, never jack's.
    let reader = temp.join("reader");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    let url = |fork: &str| format!("{}/_git/jack.store.t.{fork}.git", server.url);
    assert!(
        git_as(&bob, &reader, &["ls-remote", &url("staging-db")])
            .status
            .success()
    );
    let output = git_as(&bob, &reader, &["fetch", "-q", &url("jack-db"), "main"]);
    assert!(!output.status.success(), "bob read jack-db");

    // No push moves or deletes the record.
    let output = git_as(
        &jack,
        &reader,
        &["push", &url("staging-db"), &format!(":{record}")],
    );
    assert!(!output.status.success(), "the record was deleted by a push");
    assert!(ref_in(&home, &devs, "staging-db", record).is_some());

    // Past the client: this machine's record of the fork removed, `from`
    // dropped in one landing (still `project-devs`, so its parent's check
    // passes), then `public` in the next.
    run(
        "git",
        &home,
        &store,
        &[
            "--git-dir",
            store.join(".newgit/local/trackers.git").as_str(),
            "update-ref",
            "-d",
            "refs/newgit/forks/staging-db/local-db",
        ],
    );
    let file = store.join(".newgit/trackers/staging-db.toml");
    edit(&file, |text| text.replace("from = \"local-db\"\n", ""));
    git(&home, &store, &["commit", "-q", "-am", "Detach staging-db"]);
    newgit(&home, &store, &["merge", "main"]);
    edit(&file, |text| {
        text.replace("audience = \"project-devs\"", "audience = \"public\"")
    });
    git(
        &home,
        &store,
        &["commit", "-q", "-am", "Publish staging-db"],
    );
    newgit(&home, &store, &["merge", "main"]);
    // `public` is never encrypted, and `.env.db` held ciphertext here: the
    // client holds it back (#338) until it is released, which is part of
    // going past the client too.
    newgit(&home, &store, &["tracker", "allow-plaintext", ".env.db"]);
    newgit(&home, &store, &["checkpoint", "b"]);

    // The server still holds it to `local-db`'s audience: it stays in the
    // members' store, and someone who isn't a member reads nothing of it.
    assert_eq!(main_in(&home, &devs, "staging-db"), Some(upstream));
    assert!(!public.exists() || main_in(&home, &public, "staging-db").is_none());
    let output = git_as(&eve, &reader, &["ls-remote", &url("staging-db")]);
    assert!(!output.status.success(), "eve read staging-db");
    let output = git_as(&eve, &reader, &["fetch", "-q", &url("staging-db"), "main"]);
    assert!(!output.status.success(), "eve fetched staging-db");
}
