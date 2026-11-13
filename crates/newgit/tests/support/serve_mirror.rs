// A `newgit serve` that pulls from and exports to a mirror, with GitHub
// stood in for by bare repositories on disk: `https://github.com/` is
// rewritten to a directory by Git's own `url.<base>.insteadOf`, for the
// server and the client alike, and webhook events are signed and sent here.
// Nothing talks to github.com. `include!`d after `support/serve.rs`.

const WEBHOOK_SECRET: &str = "test-webhook-secret";

/// Where the stand-in for github.com is, and the Git config pointing there.
struct Github {
    root: Utf8PathBuf,
    gitconfig: Utf8PathBuf,
}

impl Github {
    fn new(temp: &Utf8Path) -> Self {
        let root = temp.join("github");
        std::fs::create_dir_all(&root).expect("mkdir");
        let gitconfig = temp.join("gitconfig");
        std::fs::write(
            &gitconfig,
            format!("[url \"file://{root}/\"]\n\tinsteadOf = https://github.com/\n[protocol \"file\"]\n\tallow = always\n"),
        )
        .expect("gitconfig");
        Self { root, gitconfig }
    }

    /// A bare repository standing in for `github.com/<repo>`.
    fn repo(&self, repo: &str) -> Utf8PathBuf {
        let dir = self.root.join(format!("{repo}.git"));
        if !dir.exists() {
            std::fs::create_dir_all(&dir).expect("mkdir");
            git(&self.root, &dir, &["init", "-q", "--bare", "-b", "main"]);
        }
        dir
    }

    /// `home`'s Git rewrites GitHub too.
    fn for_client(&self, home: &Utf8Path) {
        std::fs::create_dir_all(home).expect("mkdir");
        std::fs::copy(&self.gitconfig, home.join(".gitconfig")).expect("copy");
    }
}

/// `newgit serve` with `--github` naming jack/store's secret and a token,
/// `api` where GitHub's REST API is (a port nothing listens on, unless a
/// test stands one up).
fn start_mirrored(temp: &Utf8Path, github: &Github, api: &str) -> Server {
    let data = temp.join("serve");
    let config = temp.join("github.toml");
    std::fs::write(
        &config,
        format!(
            "api = \"{api}\"\n[projects.\"jack/store\"]\nwebhook_secret = \"{WEBHOOK_SECRET}\"\ntoken = \"test-token\"\n"
        ),
    )
    .expect("github.toml");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .args(["serve", "--data", data.as_str(), "--listen", "127.0.0.1:0", "--allow", "*"])
        .args(["--github", config.as_str()])
        .env("NEWGIT_SERVE_DEV_LOGIN_SECRET", SECRET)
        .env("NEWGIT_SERVE_MIRROR_WAITS", "short")
        .env("GIT_CONFIG_GLOBAL", github.gitconfig.as_str())
        .stdout(Stdio::piped())
        // What the mirror threads say, for a failing test to show.
        .stderr(std::fs::File::create(temp.join("serve.log")).expect("log"))
        .spawn()
        .expect("newgit serve starts");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("it says where it listens");
    let url = line.trim().rsplit(' ').next().expect("a URL").to_owned();
    Server { child, url, data }
}

/// A port nothing listens on: GitHub's API, unreachable.
fn no_api() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

/// jack/store, linked to the server: root (`README.md`), `serve` mounted at
/// `crates/serve` (`a.rs`, `b.rs`) shipping with root, and `t3code` at
/// `apps/t3code`, which the mirror `jack/mirror` (target `mirror`) does not
/// ship. Merged, so the server exports it.
fn mirrored_project(temp: &Utf8Path, home: &Utf8Path, server: &Server) -> Utf8PathBuf {
    let store = temp.join("store");
    std::fs::create_dir_all(store.join("crates/serve")).expect("mkdir");
    std::fs::create_dir_all(store.join("apps/t3code")).expect("mkdir");
    git(home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "store\n").expect("write");
    std::fs::write(
        store.join("crates/serve/a.rs"),
        "fn one() {}\n\nfn two() {}\n\nfn three() {}\n\nfn four() {}\n\nfn five() {}\n",
    )
    .expect("write");
    std::fs::write(store.join("crates/serve/b.rs"), "fn b() {}\n").expect("write");
    std::fs::write(store.join("apps/t3code/app.ts"), "export {}\n").expect("write");
    git(home, &store, &["add", "."]);
    git(home, &store, &["commit", "-q", "-m", "initial"]);
    newgit(home, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!("\n[workspace]\nroot = \"{}\"\n", temp.join("workspaces")));
    std::fs::write(&config, text).expect("write config");
    git(home, &store, &["add", "-A"]);
    git(home, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(home, &store, &["remote", "add", "--url", &server.url]);
    git(home, &store, &["commit", "-q", "-am", "link"]);
    newgit(home, &store, &["tracker", "create", "serve", "--git", "--path", "crates/serve"]);
    newgit(home, &store, &["tracker", "create", "t3code", "--git", "--path", "apps/t3code"]);
    set_definition_export(
        &store,
        "root",
        Some("{ url = \"github.com/jack/mirror\", name = \"mirror\", branches = true }"),
    );
    set_definition_export(&store, "access", Some("\"inherit\""));
    set_definition_export(&store, "serve", Some("\"inherit\""));
    set_definition_export(&store, "t3code", None);
    git(home, &store, &["add", "-A"]);
    git(home, &store, &["commit", "-q", "-m", "export to the mirror"]);
    newgit(home, &store, &["merge", "main"]);
    store
}

fn set_definition_export(store: &Utf8Path, tracker: &str, export: Option<&str>) {
    let path = store.join(format!(".newgit/trackers/{tracker}.toml"));
    let text = std::fs::read_to_string(&path).expect("definition");
    let mut kept: String = text
        .lines()
        .filter(|line| !line.starts_with("export "))
        .map(|line| format!("{line}\n"))
        .collect();
    if let Some(export) = export {
        kept.push_str(&format!("export = {export}\n"));
    }
    std::fs::write(&path, kept).expect("write");
}

/// A signed webhook delivery.
fn deliver(server: &Server, event: &str, delivery: &str, body: &str) -> (u16, Value) {
    let signature = newgit_serve::mirror::webhook::sign(WEBHOOK_SECRET, body.as_bytes());
    http(
        &server.url,
        "POST",
        "/v1/webhooks/github?project=jack/store",
        &[
            ("x-github-event", event),
            ("x-github-delivery", delivery),
            ("x-hub-signature-256", signature.as_str()),
            ("content-type", "application/json"),
        ],
        body,
    )
}

/// GitHub's push event for `jack/mirror`'s `main` moving to `commit`.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn push_event(commit: &str, pusher: &str) -> String {
    serde_json::json!({
        "ref": "refs/heads/main",
        "after": commit,
        "deleted": false,
        "repository": { "full_name": "jack/mirror", "private": false },
        "pusher": { "name": pusher },
    })
    .to_string()
}

/// The project's `/state`.
fn state(server: &Server, token: &str) -> Value {
    let bearer = format!("Bearer {token}");
    let (status, answer) = http(
        &server.url,
        "GET",
        "/v1/projects/jack/store/state",
        &[("authorization", bearer.as_str())],
        "",
    );
    assert_eq!(status, 200, "{answer}");
    answer
}

/// `/state` once no pull is waiting.
fn pulls_done(server: &Server, token: &str) -> Value {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        let now = state(server, token);
        if now["pulls"].as_array().is_some_and(Vec::is_empty) {
            return now;
        }
        assert!(std::time::Instant::now() < until, "the pull never ended: {now}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// How the last pull ended, by `/state`.
fn last_ended(state: &Value) -> Value {
    state["ended"]
        .as_array()
        .and_then(|ended| ended.last().cloned())
        .unwrap_or(Value::Null)
}

/// The mirror's `main`, once `done` says it is what it should be.
fn mirror_main_until(mirror: &Utf8Path, done: impl Fn(&str) -> bool) -> String {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let output = run("git", mirror, mirror, &["rev-parse", "-q", "--verify", "refs/heads/main"]);
        let tip = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() && done(&tip) {
            return tip;
        }
        if std::time::Instant::now() >= until {
            let log = mirror
                .ancestors()
                .find(|dir| dir.join("serve.log").exists())
                .and_then(|dir| std::fs::read_to_string(dir.join("serve.log")).ok())
                .unwrap_or_default();
            panic!("the mirror's main never got there (at {tip:?}); the server said:\n{log}");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// A working clone of the mirror, to make pull requests on.
fn mirror_clone(home: &Utf8Path, temp: &Utf8Path, mirror: &Utf8Path, name: &str) -> Utf8PathBuf {
    let dir = temp.join(name);
    git(home, temp, &["clone", "-q", mirror.as_str(), dir.as_str()]);
    dir
}

/// Whether `older` is an ancestor of `newer` in `dir`.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn ancestor(dir: &Utf8Path, older: &str, newer: &str) -> bool {
    run("git", dir, dir, &["merge-base", "--is-ancestor", older, newer])
        .status
        .success()
}

/// A file of `main` as the server's Git endpoint serves the whole view.
fn main_file(server: &Server, token: &str, temp: &Utf8Path, path: &str) -> String {
    let dir = temp.join(format!("view-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_nanos()));
    let url = format!("{}/jack/store.git", server.url);
    let output = git_as(token, temp, &["clone", "-q", &url, dir.as_str()]);
    assert!(output.status.success(), "clone: {}", String::from_utf8_lossy(&output.stderr));
    std::fs::read_to_string(dir.join(path)).unwrap_or_default()
}
