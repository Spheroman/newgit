// A fork's start is checked under the db lock, and a narrower fork's
// commits are copied after it is let go of (#437 review). The landing that
// makes the fork now stands only if the project's definitions did not
// change in between: here jack's first write to `staging-db` is held after
// its copy (`hold-fork-start`) while a merge changes the definitions, and it
// is refused, with no fork made. Written again, it starts as usual.

include!("support/serve.rs");
include!("support/serve_keys.rs");

fn start_with_faults(temp: &Utf8Path, faults: &Utf8Path) -> Server {
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
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("it says where it listens");
    let url = line.trim().rsplit(' ').next().expect("a URL").to_owned();
    Server { child, url, data }
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
fn a_fork_started_while_the_definitions_change_is_not_made() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let home = temp.join("jack");
    login(&server, &home, "jack");
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
    let b = Utf8PathBuf::from(newgit(&home, &store, &["status", "b", "--path"]));
    std::fs::write(b.join(".env.db"), "DATABASE_URL=postgres://staging\n").expect("write");
    newgit(&home, &store, &["tracker", "capture", "staging-db", "b"]);

    // The fork's first write, held after its copy and before its landing.
    std::fs::write(&faults, "hold-fork-start\n").expect("faults");
    let merging = {
        let (home, store) = (home.clone(), store.clone());
        std::thread::spawn(move || {
            run(
                env!("CARGO_BIN_EXE_newgit"),
                &home,
                &store,
                &["tracker", "merge", "staging-db", "b"],
            )
        })
    };
    let marker = Utf8PathBuf::from(format!("{faults}.at"));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < until,
            "the fork's start was never held"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Meanwhile the definitions change.
    newgit(
        &home,
        &store,
        &["tracker", "create", "other", "--storage", "remote"],
    );
    newgit(&home, &store, &["tracker", "track", "other", ".env.other"]);
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "other"]);
    newgit(&home, &store, &["merge", "main"]);
    std::fs::write(&faults, "").expect("faults");

    let refused = merging.join().expect("the merge ran");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !refused.status.success(),
        "a fork started on stale definitions: {said}"
    );
    assert!(said.contains("definitions"), "{said}");
    let devs = server.data.join("homes/jack/store/stores/project-devs.git");
    assert_eq!(main_in(&home, &devs, "staging-db"), None, "{said}");

    // Asked again, it is checked against the definitions as they are now.
    newgit(&home, &store, &["tracker", "merge", "staging-db", "b"]);
    assert!(main_in(&home, &devs, "staging-db").is_some());
}
