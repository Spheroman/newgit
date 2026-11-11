// More places to kill `newgit serve` in the middle of a landing (#435's
// review), beyond `serve_landing_crash`'s two. After each restart every
// reader sees whole landings only: the API's list, each tracker's Git URL, a
// fork's Git URL, and `?at=` at the branch's stamp all agree, and a tracker's
// audience is its store's.
//
// - mid-apply, after one store's refs moved and before another's;
// - after the refs and before the SQLite transaction;
// - during a store move's copy;
// - during startup replay itself (killed after the log commit, then killed
//   again while the restart puts it in place);
// - with two landings committed in one batch and neither put in place.

include!("support/serve.rs");

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

/// Wait for the server to have been killed at its fault; `context` says
/// what should have killed it.
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

/// An HTTP request that may never be answered (the server killed under it).
fn http_maybe(url: &str, method: &str, path: &str, token: &str, body: &str) -> Option<u16> {
    let host = url.trim_start_matches("http://");
    let mut stream = TcpStream::connect(host).ok()?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\nauthorization: Bearer {token}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    reply.split_whitespace().nth(1)?.parse().ok()
}

fn token_of(home: &Utf8Path) -> String {
    std::fs::read_to_string(home.join(".newgit/auth.toml"))
        .expect("auth")
        .lines()
        .find_map(|line| line.strip_prefix("token = \""))
        .map(|token| token.trim_end_matches('"').to_owned())
        .expect("a token")
}

struct World {
    temp: Utf8PathBuf,
    _guard: TempDir,
    faults: Utf8PathBuf,
    home: Utf8PathBuf,
    store: Utf8PathBuf,
    jack: String,
    eve: String,
}

/// A project whose `site` and `blog` are public, merged once, with instance
/// `feat` forking `site`.
fn world() -> (World, Server) {
    let (guard, temp) = tempdir();
    let faults = temp.join("faults");
    let server = start_with_faults(&temp, &faults).expect("starts");
    let home = temp.join("jack");
    login(&server, &home, "jack");
    login(&server, &temp.join("eve"), "eve");
    let store = temp.join("store");
    std::fs::create_dir_all(store.join("site")).expect("mkdir");
    std::fs::create_dir_all(store.join("blog")).expect("mkdir");
    git(&home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "members only\n").expect("write");
    std::fs::write(store.join("site/index.html"), "<h1>hi</h1>\n").expect("write");
    std::fs::write(store.join("blog/post.md"), "hello\n").expect("write");
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
    for tracker in ["site", "blog"] {
        newgit(
            &home,
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
    newgit(&home, &store, &["merge", "main"]);
    let jack = token_of(&home);
    let eve = token_of(&temp.join("eve"));
    let bearer = format!("Bearer {jack}");
    let auth = [("authorization", bearer.as_str())];
    let root = main_list(&server, &jack)["root"]
        .as_str()
        .expect("root")
        .to_owned();
    let (status, made) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/instances",
        &auth,
        &format!("{{\"name\":\"feat\",\"base_rev\":\"{root}\"}}"),
    );
    assert_eq!(status, 201, "{made}");
    let (status, forked) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/instances/feat/trackers/site/fork",
        &auth,
        "",
    );
    assert_eq!(status, 200, "{forked}");
    (
        World {
            temp,
            _guard: guard,
            faults,
            home,
            store,
            jack,
            eve,
        },
        server,
    )
}

fn branch(server: &Server, token: &str, name: &str) -> Value {
    let bearer = format!("Bearer {token}");
    let (status, found) = http(
        &server.url,
        "GET",
        &format!("/v1/projects/jack/store/branches/{name}"),
        &[("authorization", bearer.as_str())],
        "",
    );
    assert_eq!(status, 200, "{found}");
    found
}

fn main_list(server: &Server, token: &str) -> Value {
    branch(server, token, "main")["list"].clone()
}

/// `reference` of repo `name` as `token`'s caller reads it over Git, or
/// `None` when they may not.
fn remote_ref(
    server: &Server,
    token: &str,
    dir: &Utf8Path,
    name: &str,
    reference: &str,
) -> Option<String> {
    let url = format!("{}/_git/{name}.git", server.url);
    let listed = git_as(token, dir, &["ls-remote", &url, reference]);
    if !listed.status.success() {
        return None;
    }
    String::from_utf8_lossy(&listed.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

/// What every reader sees of `branch`, checked to agree: the API's list, the
/// list at the branch's own stamp, and each tracker's ref over Git.
fn whole(world: &World, server: &Server, name: &str) -> Value {
    let found = branch(server, &world.jack, name);
    let list = found["list"].clone();
    let stamp = found["stamp"].as_u64().expect("a stamp");
    let bearer = format!("Bearer {}", world.jack);
    let (status, at) = http(
        &server.url,
        "GET",
        &format!("/v1/projects/jack/store/branches/{name}?at={stamp}"),
        &[("authorization", bearer.as_str())],
        "",
    );
    assert_eq!(status, 200, "{at}");
    assert_eq!(at["list"], list, "?at={stamp} is the API's list");
    let reader = world.temp.join(format!("reader-{name}"));
    if !reader.exists() {
        git(&world.temp, &world.temp, &["init", "-q", reader.as_str()]);
    }
    for tracker in ["root", "site", "blog"] {
        if name != "main" && tracker != "root" {
            continue;
        }
        assert_eq!(
            remote_ref(
                server,
                &world.jack,
                &reader,
                &format!("jack.store.t.{tracker}"),
                &format!("refs/heads/{name}")
            )
            .as_deref(),
            list[tracker].as_str(),
            "{tracker}'s {name} follows the list"
        );
    }
    list
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

/// `site`'s audience as every reader sees it: whether eve (not a member)
/// reads it and its fork, and which store holds it and its fork.
fn site_is_public(world: &World, server: &Server) -> bool {
    let reader = world.temp.join("reader-eve");
    if !reader.exists() {
        git(&world.temp, &world.temp, &["init", "-q", reader.as_str()]);
    }
    let site = remote_ref(
        server,
        &world.eve,
        &reader,
        "jack.store.t.site",
        "refs/heads/main",
    );
    let fork = remote_ref(
        server,
        &world.jack,
        &reader,
        "jack.store.i.feat.site",
        "refs/heads/main",
    );
    let fork_eve = remote_ref(
        server,
        &world.eve,
        &reader,
        "jack.store.i.feat.site",
        "refs/heads/main",
    );
    assert!(fork.is_some(), "jack reads the fork");
    let stores = server.data.join("homes/jack/store/stores");
    let (public, devs) = (stores.join("public.git"), stores.join("project-devs.git"));
    let public_now = site.is_some();
    assert_eq!(
        fork_eve.is_some(),
        public_now,
        "the fork's audience is site's"
    );
    assert_eq!(has_site(&public), public_now, "site is in public.git");
    assert_eq!(has_site(&devs), !public_now, "site is in project-devs.git");
    public_now
}

/// The merge that narrows `site` to the members, adds a secret to it, and
/// changes root; killed at `fault`. Returns main's list before, and the
/// secret's blob id.
fn narrow_killed(world: &World, mut server: Server, fault: &str) -> (Value, String) {
    let before = main_list(&server, &world.jack);
    let (home, store) = (&world.home, &world.store);
    let definition = store.join(".newgit/trackers/site.toml");
    let text = std::fs::read_to_string(&definition).expect("site.toml");
    std::fs::write(
        &definition,
        text.replace("audience = \"public\"", "audience = \"project-devs\""),
    )
    .expect("write site.toml");
    std::fs::write(store.join("site/secret.txt"), "the launch date\n").expect("write");
    std::fs::write(store.join("README.md"), "members only, changed\n").expect("write");
    git(home, store, &["add", "-A"]);
    git(
        home,
        store,
        &["commit", "-q", "-m", "site is ours, with a secret"],
    );
    let secret = git(home, store, &["hash-object", "site/secret.txt"]);
    std::fs::write(&world.faults, fault).expect("arm");
    let merged = run(
        env!("CARGO_BIN_EXE_newgit"),
        home,
        store,
        &["merge", "main"],
    );
    let said = format!(
        "{}\n{}",
        String::from_utf8_lossy(&merged.stdout),
        String::from_utf8_lossy(&merged.stderr)
    );
    assert!(
        !merged.status.success(),
        "the merge finished though the server was killed:\n{said}"
    );
    killed(&mut server, &said);
    std::fs::remove_file(&world.faults).expect("disarm");
    drop(server);
    (before, secret)
}

/// After a narrowing killed past its commit point: the whole of it.
fn assert_narrowed(world: &World, server: &Server, before: &Value, secret: &str) {
    let after = whole(world, server, "main");
    assert_ne!(after["root"], before["root"], "root's move is shown");
    assert_ne!(after["site"], before["site"], "site's move is shown");
    assert_eq!(after["blog"], before["blog"]);
    assert!(!site_is_public(world, server), "site is the members' now");
    let stores = server.data.join("homes/jack/store/stores");
    assert!(has_object(&stores.join("project-devs.git"), secret));
    assert!(
        !has_object(&stores.join("public.git"), secret),
        "the secret is in public.git"
    );
}

#[test]
fn killed_during_a_store_moves_copy_the_restart_shows_the_whole_landing() {
    let (world, server) = world();
    let (before, secret) = narrow_killed(&world, server, "during-copy");
    let server = start_with_faults(&world.temp, &world.faults).expect("restarts");
    assert_narrowed(&world, &server, &before, &secret);
}

#[test]
fn killed_after_the_refs_and_before_the_rows_the_restart_shows_the_whole_landing() {
    let (world, server) = world();
    let (before, secret) = narrow_killed(&world, server, "before-rows");
    let server = start_with_faults(&world.temp, &world.faults).expect("restarts");
    assert_narrowed(&world, &server, &before, &secret);
}

#[test]
fn killed_again_while_the_restart_puts_it_in_place_the_next_shows_the_whole_landing() {
    let (world, server) = world();
    let (before, secret) = narrow_killed(&world, server, "after-log-commit");
    // The restart dies putting it in place, after its refs moved.
    std::fs::write(&world.faults, "before-rows").expect("arm");
    assert!(
        start_with_faults(&world.temp, &world.faults).is_none(),
        "the replay was killed"
    );
    std::fs::remove_file(&world.faults).expect("disarm");
    let server = start_with_faults(&world.temp, &world.faults).expect("restarts");
    assert_narrowed(&world, &server, &before, &secret);
}

#[test]
fn killed_between_two_stores_refs_the_restart_shows_the_whole_landing() {
    let (world, mut server) = world();
    let before = main_list(&server, &world.jack);
    let (home, store) = (&world.home, &world.store);
    // Root (in project-devs.git) and blog (in public.git), in one merge.
    // One commit per audience.
    std::fs::write(store.join("README.md"), "members only, again\n").expect("write");
    git(home, store, &["add", "-A"]);
    git(home, store, &["commit", "-q", "-m", "root"]);
    std::fs::write(store.join("blog/post.md"), "hello, again\n").expect("write");
    git(home, store, &["add", "-A"]);
    git(home, store, &["commit", "-q", "-m", "blog"]);
    std::fs::write(&world.faults, "between-store-commits").expect("arm");
    let merged = run(
        env!("CARGO_BIN_EXE_newgit"),
        home,
        store,
        &["merge", "main"],
    );
    let said = format!(
        "{}\n{}",
        String::from_utf8_lossy(&merged.stdout),
        String::from_utf8_lossy(&merged.stderr)
    );
    assert!(!merged.status.success(), "{said}");
    killed(&mut server, &said);
    std::fs::remove_file(&world.faults).expect("disarm");
    drop(server);
    let server = start_with_faults(&world.temp, &world.faults).expect("restarts");
    let after = whole(&world, &server, "main");
    assert_ne!(after["root"], before["root"]);
    assert_ne!(after["blog"], before["blog"]);
    assert!(site_is_public(&world, &server), "site is untouched");
    // Nothing waits in either store under a landing ref.
    let stores = server.data.join("homes/jack/store/stores");
    for name in ["project-devs.git", "public.git"] {
        let dir = stores.join(name);
        let listed = git(
            &world.temp,
            &world.temp,
            &[
                "--git-dir",
                dir.as_str(),
                "for-each-ref",
                "--format=%(refname)",
            ],
        );
        assert!(
            !listed.contains("refs/newgit/landing/"),
            "{name} still has a landing ref:\n{listed}"
        );
    }
}

#[test]
fn two_landings_committed_and_neither_in_place_are_both_put_in_place() {
    let (world, mut server) = world();
    let main = main_list(&server, &world.jack);
    let body = format!("{{\"list\":{main}}}");
    // The batch's leader waits a moment, so both are in one batch, killed
    // once it is written down.
    std::fs::write(&world.faults, "delay-batch\nafter-log-commit").expect("arm");
    let threads: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|name| {
            let (url, token, body) = (server.url.clone(), world.jack.clone(), body.clone());
            std::thread::spawn(move || {
                http_maybe(
                    &url,
                    "PUT",
                    &format!("/v1/projects/jack/store/branches/{name}"),
                    &token,
                    &body,
                )
            })
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().expect("joined"), None, "never answered");
    }
    killed(&mut server, "two branches made in one batch");
    std::fs::remove_file(&world.faults).expect("disarm");
    drop(server);
    let server = start_with_faults(&world.temp, &world.faults).expect("restarts");
    let one = whole(&world, &server, "one");
    let two = whole(&world, &server, "two");
    assert_eq!(one, main);
    assert_eq!(two, main);
    let stamps = [
        branch(&server, &world.jack, "one")["stamp"]
            .as_u64()
            .expect("stamp"),
        branch(&server, &world.jack, "two")["stamp"]
            .as_u64()
            .expect("stamp"),
    ];
    assert_eq!(
        stamps.iter().max().expect("two") - stamps.iter().min().expect("two"),
        1,
        "one after the other: {stamps:?}"
    );
    assert_eq!(whole(&world, &server, "main"), main, "main is as it was");
}
