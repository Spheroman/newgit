// A fork of an encrypted tracker on `newgit serve` (#415 with #429). The
// upstream, `local-db`, is the members' and encrypted to them (jack and
// bob). Its fork with the same audience, `staging-db`, starts at the
// upstream's commits as they are: the same ciphertext, the same recipients.
// Its fork for jack alone, `jack-db`, starts at them in jack's store (all of
// it readable by jack already), and its first sync encrypts its content to
// jack alone in a commit on top of them, so nothing it adds is readable by
// bob. Neither fork is held back by the guard on paths that held encrypted
// content: both are encrypted, at the upstream's paths.

#![allow(dead_code)]

include!("support/serve.rs");
include!("support/serve_keys.rs");

const DB_SECRET: &str = "DATABASE_URL=postgres://secret\n";

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

/// Git in the server's store at `store`, against `namespace`'s refs.
fn in_store(home: &Utf8Path, store: &Utf8Path, args: &[&str]) -> Output {
    let mut all = vec!["--git-dir", store.as_str()];
    all.extend_from_slice(args);
    run("git", home, store, &all)
}

fn main_ref(namespace: &str) -> String {
    format!("refs/namespaces/t/refs/namespaces/{namespace}/refs/heads/main")
}

fn main_in(home: &Utf8Path, store: &Utf8Path, namespace: &str) -> Option<String> {
    let output = in_store(
        home,
        store,
        &["rev-parse", "--verify", "--quiet", &main_ref(namespace)],
    );
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A file at `namespace`'s `main` in `store`, as bytes.
fn file_at(home: &Utf8Path, store: &Utf8Path, namespace: &str, path: &str) -> Vec<u8> {
    let output = in_store(
        home,
        store,
        &[
            "cat-file",
            "blob",
            &format!("{}:{path}", main_ref(namespace)),
        ],
    );
    assert!(
        output.status.success(),
        "{namespace}'s {path}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

fn is_ancestor(home: &Utf8Path, store: &Utf8Path, ancestor: &str, descendant: &str) -> bool {
    in_store(
        home,
        store,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .status
    .success()
}

#[test]
fn a_fork_of_an_encrypted_tracker_is_encrypted_to_its_own_audience() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    login(&server, &home, "jack");
    let bob_home = temp.join("bob");
    login(&server, &bob_home, "bob");
    publish_keys(&[("jack", &home), ("bob", &bob_home)]);
    let store = project(&temp, &home, &server);
    newgit(&home, &store, &["members", "add", "bob"]);
    let stores = server.data.join("homes/jack/store/stores");
    let devs = stores.join("project-devs.git");
    let jacks = stores.join("user/jack.git");

    // `local-db`'s `main` on the server: ciphertext, for jack and bob.
    newgit(&home, &store, &["spawn", "a"]);
    let a = workspace(&home, &store, "a");
    std::fs::write(a.join(".env.db"), DB_SECRET).expect("write");
    newgit(&home, &store, &["tracker", "capture", "local-db", "a"]);
    newgit(&home, &store, &["tracker", "merge", "local-db", "a"]);
    let upstream = main_in(&home, &devs, "local-db").expect("local-db's main on the server");
    let sent = file_at(&home, &devs, "local-db", ".env.db");
    assert!(sent.starts_with(b"age-encryption.org/v1\n"), "ciphertext");
    assert!(!contains(&sent, "postgres://secret"), "no plaintext");
    let recipients =
        String::from_utf8(file_at(&home, &devs, "local-db", ".newgit/recipients")).expect("utf8");
    for who in ["user:jack ", "user:bob "] {
        assert!(recipients.contains(who), "{who} in:\n{recipients}");
    }

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
    let list = newgit(&home, &store, &["tracker", "list"]);
    for fork in ["staging-db", "jack-db"] {
        let row = list.lines().find(|line| line.contains(fork)).expect("row");
        assert!(row.contains("encrypted"), "{fork} is encrypted:\n{list}");
    }
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "forks of local-db"]);
    newgit(&home, &store, &["merge", "main"]);

    // Each fork's instance starts with the upstream's content, decrypted:
    // nothing holds back a path both trackers encrypt.
    for (instance, fork) in [("b", "staging-db"), ("c", "jack-db")] {
        newgit(&home, &store, &["spawn", instance, "--use", fork]);
        let workspace = workspace(&home, &store, instance);
        assert_eq!(
            std::fs::read_to_string(workspace.join(".env.db")).expect("read"),
            DB_SECRET,
            "{fork} starts at local-db's content"
        );
        newgit(&home, &store, &["checkpoint", instance]);
    }

    // The same audience's fork starts at the upstream's commit as it is:
    // the same ciphertext, beside it in the members' store.
    assert_eq!(main_in(&home, &devs, "staging-db"), Some(upstream.clone()));
    assert_eq!(file_at(&home, &devs, "staging-db", ".env.db"), sent);

    // Jack's starts at it in jack's store: all of it readable by jack.
    assert_eq!(main_in(&home, &jacks, "jack-db"), Some(upstream.clone()));
    assert_eq!(main_in(&home, &jacks, "local-db"), None);

    // What each fork gains is encrypted to its own audience.
    for (instance, fork) in [("b", "staging-db"), ("c", "jack-db")] {
        let workspace = workspace(&home, &store, instance);
        std::fs::write(
            workspace.join(".env.db"),
            format!("DATABASE_URL=postgres://{fork}\n"),
        )
        .expect("write");
        newgit(&home, &store, &["tracker", "capture", fork, instance]);
        newgit(&home, &store, &["tracker", "merge", fork, instance]);
    }

    // The members' fork shares its upstream's history, for them both.
    let staging = main_in(&home, &devs, "staging-db").expect("staging-db's main");
    assert_ne!(staging, upstream);
    assert!(is_ancestor(&home, &devs, &upstream, &staging));
    let recipients =
        String::from_utf8(file_at(&home, &devs, "staging-db", ".newgit/recipients")).expect("utf8");
    for who in ["user:jack ", "user:bob "] {
        assert!(recipients.contains(who), "{who} in:\n{recipients}");
    }
    let file = file_at(&home, &devs, "staging-db", ".env.db");
    assert!(file.starts_with(b"age-encryption.org/v1\n"), "ciphertext");
    assert!(!contains(&file, "postgres://staging-db"), "no plaintext");

    // Jack's keeps the history it started at (readable by jack already),
    // and is encrypted to jack alone from there: bob, a member, can't read
    // what it gained even with the ciphertext in hand.
    let jack_main = main_in(&home, &jacks, "jack-db").expect("jack-db's main");
    assert!(is_ancestor(&home, &jacks, &upstream, &jack_main));
    let file = file_at(&home, &jacks, "jack-db", ".env.db");
    assert!(file.starts_with(b"age-encryption.org/v1\n"), "ciphertext");
    assert!(!contains(&file, "postgres://jack-db"), "no plaintext");
    let recipients =
        String::from_utf8(file_at(&home, &jacks, "jack-db", ".newgit/recipients")).expect("utf8");
    assert!(recipients.contains("user:jack "), "{recipients}");
    assert!(!recipients.contains("user:bob"), "bob in:\n{recipients}");
    let bob_key = std::fs::read_to_string(bob_home.join(".ssh/id_ed25519.pub")).expect("key");
    let bob_key: Vec<&str> = bob_key.split_whitespace().take(2).collect();
    assert!(!recipients.contains(&bob_key.join(" ")), "{recipients}");
    // And it never reached the members' store.
    assert_eq!(main_in(&home, &devs, "jack-db"), None);

    // A new instance of each reads what its fork gained.
    for (instance, fork) in [("d", "staging-db"), ("e", "jack-db")] {
        newgit(&home, &store, &["spawn", instance, "--use", fork]);
        let workspace = workspace(&home, &store, instance);
        assert_eq!(
            std::fs::read_to_string(workspace.join(".env.db")).expect("read"),
            format!("DATABASE_URL=postgres://{fork}\n"),
        );
    }
}
