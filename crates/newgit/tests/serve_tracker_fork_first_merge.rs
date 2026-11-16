// A fork's first `newgit tracker merge` on `newgit serve`, before anything
// else wrote to it (#437). The server makes a fork's repo on its first write,
// starting it at its upstream's `main`, so the client must lease on that
// `main`, not on none: this was `[rejected] (stale info)` until a checkpoint
// had made the repo first. And a fork re-added under its old name is still
// held to the upstream its repo was made from: the writer is told.

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

#[test]
fn a_forks_first_merge_needs_no_checkpoint_first() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    login(&server, &home, "jack");
    publish_keys(&[("jack", &home)]);
    let store = project(&temp, &home, &server);
    let devs = server.data.join("homes/jack/store/stores/project-devs.git");

    newgit(&home, &store, &["spawn", "a"]);
    let a = workspace(&home, &store, "a");
    std::fs::write(a.join(".env.db"), "DATABASE_URL=postgres://local\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "local-db", "a"]);
    newgit(&home, &store, &["tracker", "merge", "local-db", "a"]);
    let upstream = main_in(&home, &devs, "local-db").expect("local-db's main on the server");

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
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "staging-db"]);
    newgit(&home, &store, &["merge", "main"]);

    // No checkpoint: the merge is the fork's first write.
    newgit(&home, &store, &["spawn", "b", "--use", "staging-db"]);
    let b = workspace(&home, &store, "b");
    std::fs::write(b.join(".env.db"), "DATABASE_URL=postgres://staging\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "staging-db", "b"]);
    let merged = newgit(&home, &store, &["tracker", "merge", "staging-db", "b"]);
    assert!(merged.contains("moved on"), "{merged}");
    assert!(!merged.contains("served narrower"), "{merged}");

    // Its `main` moved on, from where the server started it.
    let main = main_in(&home, &devs, "staging-db").expect("staging-db's main on the server");
    assert_ne!(main, upstream);
    let ancestor = run(
        "git",
        &home,
        &devs,
        &[
            "--git-dir",
            devs.as_str(),
            "merge-base",
            "--is-ancestor",
            &upstream,
            &main,
        ],
    );
    assert!(
        ancestor.status.success(),
        "the fork's main left its upstream behind"
    );
}

#[test]
fn a_fork_re_added_under_its_old_name_says_it_is_still_held_to_its_upstream() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    login(&server, &home, "jack");
    publish_keys(&[("jack", &home)]);
    let store = project(&temp, &home, &server);

    newgit(&home, &store, &["spawn", "a"]);
    let a = workspace(&home, &store, "a");
    std::fs::write(a.join(".env.db"), "DATABASE_URL=postgres://local\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "local-db", "a"]);
    newgit(&home, &store, &["tracker", "merge", "local-db", "a"]);
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
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "staging-db"]);
    newgit(&home, &store, &["merge", "main"]);
    newgit(&home, &store, &["spawn", "b", "--use", "staging-db"]);
    newgit(&home, &store, &["checkpoint", "b"]);
    newgit(&home, &store, &["remove", "b"]);

    // Gone, then back under the same name: public now, and no fork.
    newgit(&home, &store, &["tracker", "remove", "staging-db"]);
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "no staging-db"]);
    newgit(&home, &store, &["merge", "main"]);
    newgit(
        &home,
        &store,
        &[
            "tracker",
            "create",
            "staging-db",
            "--audience",
            "public",
            "--storage",
            "remote",
        ],
    );
    newgit(
        &home,
        &store,
        &["tracker", "track", "staging-db", ".env.staging"],
    );
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "staging-db again"]);
    newgit(&home, &store, &["merge", "main"]);

    newgit(&home, &store, &["spawn", "c", "--use", "staging-db"]);
    let c = workspace(&home, &store, "c");
    std::fs::write(c.join(".env.staging"), "STAGING=1\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "staging-db", "c"]);
    let merged = newgit(&home, &store, &["tracker", "merge", "staging-db", "c"]);
    assert!(merged.contains("served narrower than defined"), "{merged}");
    assert!(merged.contains("fork of `local-db`"), "{merged}");
}
