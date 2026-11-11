// A fork's repo on `newgit serve` is made by one landing (#415 on #410's
// log): its store row, its starting refs (its upstream's `main`), and the
// server's record of its upstream (`refs/newgit/from/<upstream>`) together.
// Killed anywhere in that landing, the restart shows the fork whole, with
// its starting refs and its record, or not at all:
//
// - after the log commit, before anything is put in place: replayed, whole;
// - after its refs moved, before its row (a narrower fork, whose commits
//   were copied into its own store first): replayed, whole;
// - before the log commit: not there, and the next write makes it whole.

include!("support/serve.rs");
include!("support/serve_keys.rs");

/// `newgit serve` on `temp`'s data directory, with faults read from
/// `faults`; `None` when it stopped before it listened.
fn start_with_faults(temp: &Utf8Path, faults: &Utf8Path) -> Option<Server> {
    let data = temp.join("serve");
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .args([
            "serve",
            "--data",
            data.as_str(),
            "--listen",
            "127.0.0.1:0",
            "--allow",
            "*",
        ])
        .env("NEWGIT_SERVE_DEV_LOGIN_SECRET", SECRET)
        .env("NEWGIT_SERVE_FAULT_FILE", faults.as_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("newgit serve starts");
    let mut line = String::new();
    let _ = BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut line);
    let url = line
        .trim()
        .rsplit(' ')
        .next()
        .unwrap_or_default()
        .to_owned();
    if !url.starts_with("http://") {
        let status = child.wait().expect("it ends");
        assert!(!status.success(), "it stopped, killed");
        return None;
    }
    Some(Server { child, url, data })
}

/// Wait for the server to have been killed at its fault.
fn killed(server: &mut Server, context: &str) {
    for _ in 0..600 {
        if let Some(status) = server.child.try_wait().expect("polls") {
            assert!(!status.success(), "killed, not stopped");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the server was never killed: {context}");
}

struct World {
    temp: Utf8PathBuf,
    _guard: TempDir,
    faults: Utf8PathBuf,
    home: Utf8PathBuf,
    store: Utf8PathBuf,
    jack: String,
    /// `local-db`'s `main` on the server.
    upstream: String,
}

/// A project whose `local-db` (the members', encrypted, synced) has a
/// `main` on the server, and two forks of it defined: `staging-db` (the
/// members') and `jack-db` (jack's), each with an instance using it that
/// has not synced yet, so neither fork's repo is made.
fn world() -> (World, Server) {
    let (guard, temp) = tempdir();
    let faults = temp.join("faults");
    let server = start_with_faults(&temp, &faults).expect("starts");
    let home = temp.join("jack");
    let jack = login(&server, &home, "jack");
    publish_keys(&[("jack", &home)]);
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "members only\n").expect("write");
    git(&home, &store, &["add", "."]);
    git(&home, &store, &["commit", "-q", "-m", "initial"]);
    newgit(&home, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(&home, &store, &["remote", "add", "--url", &server.url]);
    git(&home, &store, &["commit", "-q", "-am", "link"]);
    newgit(
        &home,
        &store,
        &["tracker", "create", "local-db", "--storage", "remote"],
    );
    newgit(&home, &store, &["tracker", "track", "local-db", ".env.db"]);
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "local-db"]);
    newgit(&home, &store, &["merge", "main"]);
    newgit(&home, &store, &["spawn", "a"]);
    let a = Utf8PathBuf::from(newgit(&home, &store, &["status", "a", "--path"]));
    std::fs::write(a.join(".env.db"), "DATABASE_URL=postgres://secret\n").expect("write");
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
    }
    let world = World {
        temp,
        _guard: guard,
        faults,
        home,
        store,
        jack,
        upstream: String::new(),
    };
    let upstream = ref_in(&world, "project-devs.git", "local-db", "refs/heads/main")
        .expect("local-db's main on the server");
    (World { upstream, ..world }, server)
}

/// The store each fork's repo is made in.
fn store_of(fork: &str) -> &'static str {
    match fork {
        "jack-db" => "user/jack.git",
        _ => "project-devs.git",
    }
}

/// `reference` of `tracker`'s namespace in the server's store `store`, read
/// from its data directory on disk (whether it runs or not).
fn ref_in(world: &World, store: &str, tracker: &str, reference: &str) -> Option<String> {
    let dir = world.temp.join("serve/homes/jack/store/stores").join(store);
    if !dir.exists() {
        return None;
    }
    let output = run(
        "git",
        &world.home,
        &world.temp,
        &[
            "--git-dir",
            dir.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/namespaces/t/refs/namespaces/{tracker}/{reference}"),
        ],
    );
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `fork`'s `main` as jack reads it over Git, or `None` when it is not
/// served (its repo is not made).
fn served_main(world: &World, server: &Server, fork: &str) -> Option<String> {
    let reader = world.temp.join("reader");
    if !reader.exists() {
        git(&world.home, &world.temp, &["init", "-q", reader.as_str()]);
    }
    let url = format!("{}/_git/jack.store.t.{fork}.git", server.url);
    let listed = git_as(
        &world.jack,
        &reader,
        &["ls-remote", &url, "refs/heads/main"],
    );
    if !listed.status.success() {
        return None;
    }
    String::from_utf8_lossy(&listed.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

/// The first sync of `fork`'s instance, which makes the fork's repo, killed
/// at `fault`.
fn first_sync_killed(world: &World, mut server: Server, instance: &str, fault: &str) {
    std::fs::write(&world.faults, fault).expect("arm");
    let synced = run(
        env!("CARGO_BIN_EXE_newgit"),
        &world.home,
        &world.store,
        &["checkpoint", instance],
    );
    let said = format!(
        "{}\n{}",
        String::from_utf8_lossy(&synced.stdout),
        String::from_utf8_lossy(&synced.stderr)
    );
    killed(&mut server, &said);
    std::fs::remove_file(&world.faults).expect("disarm");
}

/// `fork` whole: served, at its upstream's `main`, with its record.
fn assert_whole(world: &World, server: &Server, fork: &str) {
    let store = store_of(fork);
    assert_eq!(
        ref_in(world, store, fork, "refs/heads/main").as_deref(),
        Some(world.upstream.as_str()),
        "{fork} starts at local-db's main"
    );
    assert!(
        ref_in(world, store, fork, "refs/newgit/from/local-db").is_some(),
        "{fork} has its record"
    );
    assert_eq!(
        served_main(world, server, fork).as_deref(),
        Some(world.upstream.as_str()),
        "{fork} is served"
    );
}

/// `fork` not there at all: not served, no starting ref, no record.
fn assert_absent(world: &World, server: &Server, fork: &str) {
    let store = store_of(fork);
    assert_eq!(served_main(world, server, fork), None, "{fork} is served");
    assert_eq!(
        ref_in(world, store, fork, "refs/heads/main"),
        None,
        "{fork} has a main"
    );
    assert_eq!(
        ref_in(world, store, fork, "refs/newgit/from/local-db"),
        None,
        "{fork} has a record"
    );
}

#[test]
fn killed_after_its_landing_is_written_a_fork_comes_back_whole() {
    let (world, server) = world();
    first_sync_killed(&world, server, "b", "after-log-commit");
    // Written down, nothing put in place: the restart's replay does it.
    assert_eq!(
        ref_in(&world, "project-devs.git", "staging-db", "refs/heads/main"),
        None
    );
    let server = start_with_faults(&world.temp, &world.faults).expect("restarts");
    assert_whole(&world, &server, "staging-db");
}

#[test]
fn killed_after_a_narrower_forks_refs_and_before_its_row_it_comes_back_whole() {
    let (world, server) = world();
    first_sync_killed(&world, server, "c", "before-rows");
    // Its refs moved before it was killed; its row did not.
    assert_eq!(
        ref_in(&world, "user/jack.git", "jack-db", "refs/heads/main").as_deref(),
        Some(world.upstream.as_str())
    );
    let server = start_with_faults(&world.temp, &world.faults).expect("restarts");
    assert_whole(&world, &server, "jack-db");
    // Never in the members' store.
    assert_eq!(
        ref_in(&world, "project-devs.git", "jack-db", "refs/heads/main"),
        None
    );
}

#[test]
fn killed_before_its_landing_is_written_a_fork_is_not_there_until_the_next_write() {
    let (world, server) = world();
    first_sync_killed(&world, server, "b", "before-log-commit");
    let server = start_with_faults(&world.temp, &world.faults).expect("restarts");
    assert_absent(&world, &server, "staging-db");
    // The next write makes it, whole: what a push asks first.
    let writer = world.temp.join("writer");
    git(&world.home, &world.temp, &["init", "-q", writer.as_str()]);
    git(
        &world.home,
        &writer,
        &["commit", "-q", "--allow-empty", "-m", "x"],
    );
    let url = format!("{}/_git/jack.store.t.staging-db.git", server.url);
    let _ = git_as(
        &world.jack,
        &writer,
        &["push", "--dry-run", &url, "HEAD:refs/heads/scratch"],
    );
    assert_whole(&world, &server, "staging-db");
}
