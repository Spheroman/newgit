// The landing log is the commit point (#410). `newgit serve` is killed in the
// middle of a landing that moves root, moves `site`, narrows `site` from
// `public` to `project-devs` (a store move), and adds a secret to it, then
// started again on the same data directory. Every reader then sees the whole
// landing or none of it:
//
// - killed after the log commit and before anything follows it, the restart
//   puts it in place: the API's list, root's and site's refs, and site's
//   store all say the new state, and a non-member can no longer read `site`;
// - killed before the log commit, nothing of it is anywhere: the old list,
//   the old refs, `site` still public, its secret in no store.

include!("support/serve.rs");

/// `newgit serve` on `temp`'s data directory, with faults read from `faults`.
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

/// A project whose `site` and `blog` are public, merged once.
fn project(temp: &Utf8Path, home: &Utf8Path, server: &Server) -> Utf8PathBuf {
    let store = temp.join("store");
    std::fs::create_dir_all(store.join("site")).expect("mkdir");
    std::fs::create_dir_all(store.join("blog")).expect("mkdir");
    git(home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "members only\n").expect("write");
    std::fs::write(store.join("site/index.html"), "<h1>hi</h1>\n").expect("write");
    std::fs::write(store.join("blog/post.md"), "hello\n").expect("write");
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
    for tracker in ["site", "blog"] {
        newgit(
            home,
            &store,
            &[
                "tracker",
                "create",
                tracker,
                "--git",
                "--audience",
                "public",
                "--path",
                tracker,
            ],
        );
    }
    newgit(home, &store, &["merge", "main"]);
    store
}

/// main's list, as the API answers it.
fn main_list(server: &Server, token: &str) -> Value {
    let bearer = format!("Bearer {token}");
    let (status, main) = http(
        &server.url,
        "GET",
        "/v1/projects/jack/store/branches/main",
        &[("authorization", bearer.as_str())],
        "",
    );
    assert_eq!(status, 200, "{main}");
    main["list"].clone()
}

/// `refs/heads/main` of repo `name` as `token`'s caller reads it over Git,
/// or `None` when they may not.
fn remote_main(server: &Server, token: &str, dir: &Utf8Path, name: &str) -> Option<String> {
    let url = format!("{}/_git/{name}.git", server.url);
    let listed = git_as(token, dir, &["ls-remote", &url, "refs/heads/main"]);
    if !listed.status.success() {
        return None;
    }
    String::from_utf8_lossy(&listed.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

fn has_object(store: &Utf8Path, object: &str) -> bool {
    run(
        "git",
        store,
        store,
        &["--git-dir", store.as_str(), "cat-file", "-e", object],
    )
    .status
    .success()
}

fn has_site(store: &Utf8Path) -> bool {
    run(
        "git",
        store,
        store,
        &[
            "--git-dir",
            store.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/namespaces/t/refs/namespaces/site/refs/heads/main",
        ],
    )
    .status
    .success()
}

/// Kill the server at `fault` during a merge that narrows `site`, adds a
/// secret to it, and changes root; start it again. Returns what readers saw
/// before and after, and the secret's blob id.
struct Crashed {
    temp: Utf8PathBuf,
    _guard: TempDir,
    server: Server,
    jack: String,
    eve: String,
    before: Value,
    secret: String,
}

fn crash_at(fault: &str) -> Crashed {
    let (guard, temp) = tempdir();
    let faults = temp.join("faults");
    let mut server = start_with_faults(&temp, &faults);
    let home = temp.join("jack");
    login(&server, &home, "jack");
    login(&server, &temp.join("eve"), "eve");
    let store = project(&temp, &home, &server);
    let jack = std::fs::read_to_string(home.join(".newgit/auth.toml"))
        .expect("auth")
        .lines()
        .find_map(|line| line.strip_prefix("token = \""))
        .map(|token| token.trim_end_matches('"').to_owned())
        .expect("jack's token");
    let eve = std::fs::read_to_string(temp.join("eve/.newgit/auth.toml"))
        .expect("auth")
        .lines()
        .find_map(|line| line.strip_prefix("token = \""))
        .map(|token| token.trim_end_matches('"').to_owned())
        .expect("eve's token");
    let before = main_list(&server, &jack);

    let definition = store.join(".newgit/trackers/site.toml");
    let text = std::fs::read_to_string(&definition).expect("site.toml");
    std::fs::write(
        &definition,
        text.replace("audience = \"public\"", "audience = \"project-devs\""),
    )
    .expect("write site.toml");
    std::fs::write(store.join("site/secret.txt"), "the launch date\n").expect("write");
    std::fs::write(store.join("README.md"), "members only, changed\n").expect("write");
    git(&home, &store, &["add", "-A"]);
    git(
        &home,
        &store,
        &["commit", "-q", "-m", "site is ours, with a secret"],
    );
    let secret = git(&home, &store, &["hash-object", "site/secret.txt"]);

    std::fs::write(&faults, fault).expect("arm the fault");
    let merged = run(
        env!("CARGO_BIN_EXE_newgit"),
        &home,
        &store,
        &["merge", "main"],
    );
    assert!(
        !merged.status.success(),
        "the merge finished though the server was killed:\n{}",
        String::from_utf8_lossy(&merged.stdout)
    );
    let status = server.child.wait().expect("the server ends");
    assert!(!status.success(), "the server was killed, not stopped");
    std::fs::remove_file(&faults).expect("disarm");
    drop(server);

    let server = start_with_faults(&temp, &faults);
    Crashed {
        temp,
        _guard: guard,
        server,
        jack,
        eve,
        before,
        secret,
    }
}

#[test]
fn killed_after_the_log_commit_the_restart_shows_the_whole_landing() {
    let crashed = crash_at("after-log-commit");
    let Crashed {
        temp,
        server,
        jack,
        eve,
        before,
        secret,
        ..
    } = &crashed;
    let after = main_list(server, jack);
    assert_ne!(after["root"], before["root"], "root's move is shown");
    assert_ne!(after["site"], before["site"], "site's move is shown");
    let reader = temp.join("reader");
    git(temp, temp, &["init", "-q", reader.as_str()]);
    assert_eq!(
        remote_main(server, jack, &reader, "jack.store.t.root").as_deref(),
        after["root"].as_str(),
        "root's ref follows the list"
    );
    assert_eq!(
        remote_main(server, jack, &reader, "jack.store.t.site").as_deref(),
        after["site"].as_str(),
        "site's ref follows the list"
    );
    // site moved to the members' store with it, its secret only there.
    let stores = server.data.join("homes/jack/store/stores");
    let (public, devs) = (stores.join("public.git"), stores.join("project-devs.git"));
    assert!(has_site(&devs) && !has_site(&public));
    assert!(has_object(&devs, secret) && !has_object(&public, secret));
    assert_eq!(
        remote_main(server, eve, &reader, "jack.store.t.site"),
        None,
        "a non-member still reads site"
    );
    assert!(
        remote_main(server, eve, &reader, "jack.store.t.blog").is_some(),
        "blog is still public"
    );
}

#[test]
fn killed_before_the_log_commit_the_restart_shows_none_of_it() {
    let crashed = crash_at("before-log-commit");
    let Crashed {
        temp,
        server,
        jack,
        eve,
        before,
        secret,
        ..
    } = &crashed;
    assert_eq!(&main_list(server, jack), before, "nothing of it is listed");
    let reader = temp.join("reader");
    git(temp, temp, &["init", "-q", reader.as_str()]);
    assert_eq!(
        remote_main(server, jack, &reader, "jack.store.t.root").as_deref(),
        before["root"].as_str()
    );
    assert_eq!(
        remote_main(server, eve, &reader, "jack.store.t.site").as_deref(),
        before["site"].as_str(),
        "site is public, where it was"
    );
    let stores = server.data.join("homes/jack/store/stores");
    let (public, devs) = (stores.join("public.git"), stores.join("project-devs.git"));
    assert!(has_site(&public) && !has_site(&devs));
    assert!(
        !has_object(&public, secret),
        "the secret of a narrowing that never landed is in public.git"
    );
}
