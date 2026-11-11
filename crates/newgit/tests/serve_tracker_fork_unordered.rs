// A fork the server can't hold to its upstream without moving it somewhere
// it wasn't is not defined, never moved (#433 review). Jack's `user:jack`
// fork of `local-db`, past the client (its `from` dropped and this
// machine's record removed, as an older client would), and then `local-db`
// narrowed to `user:bob`: `user:jack` and `user:bob` are not ordered, so
// bob reads nothing of `jack-db`, and jack's commits stay in jack's store.

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
fn a_fork_for_one_user_never_moves_to_another_when_its_upstream_narrows() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let _jack = login(&server, &home, "jack");
    let bob = login(&server, &temp.join("bob"), "bob");
    publish_keys(&[("jack", &home), ("bob", &temp.join("bob"))]);
    let store = project(&temp, &home, &server);
    newgit(&home, &store, &["members", "add", "bob"]);
    let stores = server.data.join("homes/jack/store/stores");
    let jacks = stores.join("user/jack.git");
    let bobs = stores.join("user/bob.git");

    newgit(&home, &store, &["spawn", "a"]);
    let a = workspace(&home, &store, "a");
    std::fs::write(a.join(".env.db"), "DATABASE_URL=postgres://shared\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "local-db", "a"]);
    newgit(&home, &store, &["tracker", "merge", "local-db", "a"]);

    newgit(
        &home,
        &store,
        &[
            "tracker",
            "create",
            "jack-db",
            "--role",
            "db",
            "--from",
            "local-db",
            "--audience",
            "user:jack",
        ],
    );
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "jack-db"]);
    newgit(&home, &store, &["merge", "main"]);
    newgit(&home, &store, &["spawn", "c", "--use", "jack-db"]);
    newgit(&home, &store, &["checkpoint", "c"]);
    let c = workspace(&home, &store, "c");
    std::fs::write(c.join(".env.db"), "DATABASE_URL=postgres://jacks-own\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "jack-db", "c"]);
    newgit(&home, &store, &["tracker", "merge", "jack-db", "c"]);
    newgit(&home, &store, &["checkpoint", "c"]);
    let mine = main_in(&home, &jacks, "jack-db").expect("jack-db's main on the server");
    assert!(ref_in(&home, &jacks, "jack-db", "refs/newgit/from/local-db").is_some());

    // Past the client: this machine's record removed and `from` dropped.
    run(
        "git",
        &home,
        &store,
        &[
            "--git-dir",
            store.join(".newgit/local/trackers.git").as_str(),
            "update-ref",
            "-d",
            "refs/newgit/forks/jack-db/local-db",
        ],
    );
    edit(&store.join(".newgit/trackers/jack-db.toml"), |text| {
        text.replace("from = \"local-db\"\n", "")
    });
    git(&home, &store, &["commit", "-q", "-am", "Detach jack-db"]);
    newgit(&home, &store, &["merge", "main"]);

    // The owner narrows `local-db` to bob.
    edit(&store.join(".newgit/trackers/local-db.toml"), |text| {
        text.replace("audience = \"project-devs\"", "audience = \"user:bob\"")
    });
    git(&home, &store, &["commit", "-q", "-am", "local-db is bob's"]);
    newgit(&home, &store, &["merge", "main"]);

    // `jack-db` is not defined: nothing of it moved to bob's store, jack's
    // commit is where it was, and bob reads nothing of it.
    assert_eq!(main_in(&home, &jacks, "jack-db"), Some(mine.clone()));
    assert!(!bobs.exists() || main_in(&home, &bobs, "jack-db").is_none());
    let reader = temp.join("reader");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    let url = format!("{}/_git/jack.store.t.jack-db.git", server.url);
    let output = git_as(&bob, &reader, &["fetch", "-q", &url, "main"]);
    assert!(!output.status.success(), "bob fetched jack-db");
    let output = git_as(&bob, &reader, &["ls-remote", &url]);
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(!listed.contains(&mine), "bob listed jack-db: {listed}");
}
