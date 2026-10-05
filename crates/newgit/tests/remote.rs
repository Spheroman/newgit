//! A project linked to a newgit remote, through the real binary. The remote
//! is a fake on 127.0.0.1 that speaks the Worker's JSON API, with local bare
//! repositories standing in for Artifacts repos — so the binary's own HTTP
//! client, its credential handling, and the out-of-process push hook are all
//! exercised. `HOME` is a temp directory, so no real login is ever read.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};

// ── Fake remote ────────────────────────────────────────────────────────────

/// A token the CLI will not try to renew: its payload says it expires in 2099.
fn fake_token(id: &str) -> String {
    let payload = format!("{{\"jti\":\"{id}\",\"exp\":4070908800}}");
    let encoded: String = base64_url(payload.as_bytes());
    format!("ngt_{encoded}.sig")
}

fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            }
        }
    }
    out
}

#[derive(Default)]
struct State {
    /// `(method, path, bearer token)` for every request, in order.
    requests: Vec<(String, String, String)>,
    /// Instance name → how many more times to answer "pending" before "ready".
    pending_polls: BTreeMap<String, u32>,
    checkpoints: BTreeMap<(String, String), Value>,
    /// Who checkpointed what, for `createdBy`.
    checkpoint_authors: BTreeMap<(String, String), String>,
    /// Instance token → the login it acts as.
    instance_tokens: BTreeMap<String, String>,
}

/// Ana's login token; every other login token is jack's.
const ANA_TOKEN_ID: &str = "tok_ana";

fn login_of(state: &State, token: &str) -> String {
    if token == fake_token(ANA_TOKEN_ID) {
        return "ana".to_owned();
    }
    state
        .instance_tokens
        .get(token)
        .cloned()
        .unwrap_or_else(|| "jack".to_owned())
}

struct FakeRemote {
    url: String,
    repos: Utf8PathBuf,
    state: Arc<Mutex<State>>,
}

const LOGIN_TOKEN_ID: &str = "tok_login";

impl FakeRemote {
    fn start(repos: Utf8PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("addr").port()
        );
        let state = Arc::new(Mutex::new(State::default()));
        let (thread_state, thread_repos) = (state.clone(), repos.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut parts = request_line.split_whitespace();
                let (method, path) = (
                    parts.next().unwrap_or("").to_owned(),
                    parts.next().unwrap_or("").to_owned(),
                );
                let mut length = 0;
                let mut token = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header");
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap_or((line, ""));
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse().unwrap_or(0),
                        "authorization" => {
                            token = value.trim().trim_start_matches("Bearer ").to_owned()
                        }
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("body");
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let (status, reply) =
                    route(&thread_state, &thread_repos, &method, &path, &token, &body);
                let text = reply.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        Self { url, repos, state }
    }

    fn requests(&self) -> Vec<(String, String, String)> {
        self.state.lock().expect("state").requests.clone()
    }

    fn repo(&self, name: &str) -> Utf8PathBuf {
        self.repos.join(format!("{name}.git"))
    }
}

fn route(
    state: &Mutex<State>,
    repos: &Utf8Path,
    method: &str,
    path: &str,
    token: &str,
    body: &Value,
) -> (u16, Value) {
    let mut state = state.lock().expect("state");
    state
        .requests
        .push((method.to_owned(), path.to_owned(), token.to_owned()));
    let caller = login_of(&state, token);
    let repo_url = |name: &str| repos.join(format!("{name}.git")).to_string();
    let exists = |name: &str| repos.join(format!("{name}.git")).exists();
    let segments: Vec<&str> = path.trim_start_matches('/').split(['/', '?']).collect();
    let credential = |repo: &str| json!({ "repo": repo, "remote": repo_url(repo), "username": "x", "password": "pw", "expires_at": "2099-01-01T00:00:00Z" });
    match (method, segments.as_slice()) {
        ("GET", ["v1", "me"]) => (
            200,
            json!({ "login": caller, "kind": "login", "token_id": "tok" }),
        ),
        ("POST", ["v1", "projects"]) => {
            let name = body["name"].as_str().unwrap_or_default();
            let src = format!("{name}.src");
            if exists(&src) {
                return (
                    409,
                    json!({ "error": format!("project `{name}` already exists") }),
                );
            }
            git(
                repos,
                &["init", "-q", "--bare", "-b", "main", &repo_url(&src)],
            );
            (
                201,
                json!({
                    "name": name, "owner": caller, "src": { "repo": src, "remote": repo_url(&src) },
                    "seed_credential": credential(&src),
                }),
            )
        }
        ("GET", ["v1", "projects", project]) => (
            200,
            json!({
                "name": project, "owner": "jack", "members": [{ "login": "ana" }, { "login": "jack" }],
                "src": { "repo": format!("{project}.src"), "remote": repo_url(&format!("{project}.src")) },
            }),
        ),
        ("POST", ["v1", "projects", project, "instances"]) => {
            let name = body["name"].as_str().unwrap_or_default();
            let fork = format!("{project}.i.{name}");
            git(
                repos,
                &[
                    "clone",
                    "-q",
                    "--bare",
                    &repo_url(&format!("{project}.src")),
                    &repo_url(&fork),
                ],
            );
            // The first push polls once while the "fork" is still being made.
            state.pending_polls.insert(name.to_owned(), 1);
            let instance_token = fake_token(&format!("tok_instance_{name}"));
            state
                .instance_tokens
                .insert(instance_token.clone(), caller.clone());
            (
                201,
                json!({
                    "name": name, "owner": caller, "agent": null, "baseRev": body["base_rev"], "headRev": null,
                    "forkState": "pending", "repo": fork, "remote": repo_url(&fork),
                    "trunk": { "repo": format!("{project}.src"), "remote": repo_url(&format!("{project}.src")) },
                    "token": instance_token, "token_id": format!("tok_instance_{name}"),
                }),
            )
        }
        ("GET", ["v1", "projects", project, "instances", name]) => {
            let remaining = state.pending_polls.get(*name).copied().unwrap_or(0);
            if remaining > 0 {
                state
                    .pending_polls
                    .insert((*name).to_owned(), remaining - 1);
            }
            let fork = format!("{project}.i.{name}");
            let fork_state = if remaining > 0 { "pending" } else { "ready" };
            (
                200,
                json!({
                    "name": name, "owner": caller, "baseRev": "x", "forkState": fork_state,
                    "repo": fork, "remote": repo_url(&fork),
                }),
            )
        }
        ("DELETE", ["v1", "projects", project, "instances", name]) => {
            let _ = std::fs::remove_dir_all(repo_url(&format!("{project}.i.{name}")));
            (200, json!({ "name": name, "removed": true }))
        }
        ("POST", ["v1", "projects", _, "credentials"]) => {
            let repo = body["repo"].as_str().unwrap_or_default();
            // Like the real remote: a per-user lane is its owner's alone.
            if let Some(owner) = repo.split('.').nth(3)
                && owner != caller
            {
                return (
                    403,
                    json!({ "error": format!("{repo} is {owner}'s own; each user reads only their own") }),
                );
            }
            if !exists(repo) {
                // A tracker repo is created on first write; nothing else is.
                if repo.contains(".t.") && body["scope"] == "write" {
                    git(repos, &["init", "-q", "--bare", &repo_url(repo)]);
                } else {
                    return (
                        404,
                        json!({ "error": format!("{repo} does not exist yet") }),
                    );
                }
            }
            (200, credential(repo))
        }
        ("PUT", ["v1", "projects", _, "instances", name, "head"]) => {
            (200, json!({ "name": name, "headRev": body["head_rev"] }))
        }
        ("PUT", ["v1", "projects", _, "checkpoints", instance, id]) => {
            let key = ((*instance).to_owned(), (*id).to_owned());
            state
                .checkpoints
                .insert(key.clone(), body["record"].clone());
            state.checkpoint_authors.insert(key, caller.clone());
            (
                201,
                json!({ "instance": instance, "id": id, "createdBy": caller, "createdAt": "now", "record": body["record"] }),
            )
        }
        ("GET", ["v1", "projects", _, "checkpoints", instance, id]) => {
            let key = ((*instance).to_owned(), (*id).to_owned());
            match state.checkpoints.get(&key) {
                Some(record) => (
                    200,
                    json!({
                        "instance": instance, "id": id, "createdBy": state.checkpoint_authors[&key],
                        "agent": null, "createdAt": "now", "record": record,
                    }),
                ),
                None => (404, json!({ "error": "no such checkpoint" })),
            }
        }
        _ => (
            404,
            json!({ "error": format!("fake remote has no route for {method} {path}") }),
        ),
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn git_output(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs")
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-remote-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

struct Fixture {
    temp: Utf8PathBuf,
    store: Utf8PathBuf,
    remote: FakeRemote,
}

impl Fixture {
    fn newgit(&self, dir: &Utf8Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(dir.as_str())
            .args(args)
            .env("HOME", self.temp.join("home").as_str())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("newgit runs")
    }

    fn ok(&self, dir: &Utf8Path, args: &[&str]) -> String {
        let output = self.newgit(dir, args);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "newgit {args:?} failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }
}

/// A store, logged in to the fake remote, linked with `newgit remote add`.
fn setup(temp: &Utf8Path) -> Fixture {
    let repos = temp.join("artifacts");
    std::fs::create_dir_all(&repos).expect("mkdir");
    let remote = FakeRemote::start(repos);

    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    git(&store, &["add", "."]);
    git(&store, &["commit", "-q", "-m", "initial"]);

    // A saved login, as `newgit login` writes it.
    let home = temp.join("home/.newgit");
    std::fs::create_dir_all(&home).expect("mkdir");
    std::fs::write(
        home.join("auth.toml"),
        format!(
            "[remotes.\"{}\"]\nlogin = \"jack\"\ntoken = \"{}\"\ntoken_id = \"{LOGIN_TOKEN_ID}\"\n",
            remote.url,
            fake_token(LOGIN_TOKEN_ID)
        ),
    )
    .expect("write auth");

    let fixture = Fixture {
        temp: temp.to_path_buf(),
        store,
        remote,
    };
    fixture.ok(&fixture.store, &["init"]);
    let config = fixture.store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    fixture.ok(
        &fixture.store,
        &["remote", "add", "--url", &fixture.remote.url],
    );
    fixture
}

fn workspace_of(fixture: &Fixture, name: &str) -> Utf8PathBuf {
    Utf8PathBuf::from(
        fixture
            .ok(&fixture.store, &["status", name, "--path"])
            .trim(),
    )
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn remote_add_creates_the_project_and_seeds_trunk_with_this_history() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let trunk = fixture.remote.repo("store.src");
    assert_eq!(
        git(&trunk, &["rev-parse", "main"]),
        git(&fixture.store, &["rev-parse", "main"])
    );
    let config =
        std::fs::read_to_string(fixture.store.join(".newgit/config.toml")).expect("config");
    assert!(
        config.contains(&format!("url = \"{}\"", fixture.remote.url)),
        "{config}"
    );
}

#[test]
fn spawn_registers_the_instance_and_gives_the_workspace_its_own_token() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");

    let token_path = workspace.join(".git/newgit/token");
    assert_eq!(
        std::fs::read_to_string(&token_path).expect("token"),
        fake_token("tok_instance_feature-a")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&token_path)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the workspace token is readable by its owner only"
        );
    }
    // Never committed: it lives inside .git/.
    assert!(git(&workspace, &["status", "--porcelain"]).is_empty());

    // `origin` fetches trunk.
    assert_eq!(
        git(&workspace, &["config", "remote.origin.url"]),
        fixture.remote.repo("store.src").as_str()
    );

    let record = std::fs::read_to_string(fixture.store.join(".newgit/branches/feature-a.toml"))
        .expect("record");
    assert!(record.contains("repo = \"store.i.feature-a\""), "{record}");
}

#[test]
fn a_push_lands_in_the_instance_fork_authenticated_as_the_instance() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");

    std::fs::write(workspace.join("change.txt"), "agent work\n").expect("write");
    git(&workspace, &["add", "change.txt"]);
    git(&workspace, &["commit", "-q", "-m", "agent work"]);
    let head = git(&workspace, &["rev-parse", "HEAD"]);
    let push = git_output(&workspace, &["push", "-q", "origin", "feature-a"]);
    assert!(
        push.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    let fork = fixture.remote.repo("store.i.feature-a");
    assert_eq!(
        git(&fork, &["rev-parse", "feature-a"]),
        head,
        "the push reached the fork"
    );
    let trunk = fixture.remote.repo("store.src");
    assert!(
        git_output(&trunk, &["rev-parse", "--verify", "--quiet", "feature-a"])
            .stdout
            .is_empty(),
        "not trunk"
    );

    let instance_token = fake_token("tok_instance_feature-a");
    let requests = fixture.remote.requests();
    // It waited for the fork, then minted the fork's password on the
    // workspace's own token — not the owner's login.
    assert!(requests.iter().any(|(m, p, t)| m == "GET"
        && p.ends_with("/instances/feature-a")
        && *t == instance_token));
    assert!(
        requests
            .iter()
            .any(|(m, p, t)| m == "POST" && p.ends_with("/credentials") && *t == instance_token)
    );
}

#[test]
fn remove_takes_the_instance_off_the_remote() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    assert!(fixture.remote.repo("store.i.feature-a").exists());
    fixture.ok(&fixture.store, &["remove", "feature-a"]);
    assert!(
        !fixture.remote.repo("store.i.feature-a").exists(),
        "the fork is gone"
    );
    let login = fake_token(LOGIN_TOKEN_ID);
    assert!(
        fixture
            .remote
            .requests()
            .iter()
            .any(|(m, p, t)| m == "DELETE" && p.ends_with("/instances/feature-a") && *t == login)
    );
}

#[test]
fn spawn_still_works_when_the_remote_is_unreachable() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    // Point the project at a port nothing listens on.
    let config = fixture.store.join(".newgit/config.toml");
    let text = std::fs::read_to_string(&config)
        .expect("config")
        .replace(&fixture.remote.url, "http://127.0.0.1:9");
    std::fs::write(&config, text).expect("write");
    let output = fixture.newgit(&fixture.store, &["spawn", "offline"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        all.contains("not on the remote"),
        "the spawn says so:\n{all}"
    );
}

#[test]
fn a_checkpoint_syncs_source_remote_trackers_and_then_the_record() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    // One tracker synced to the remote, one kept on this machine.
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "runtime-env",
            "--audience",
            "user",
            "--storage",
            "remote",
        ],
    );
    fixture.ok(
        &fixture.store,
        &["tracker", "track", "runtime-env", ".env.local"],
    );
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "scratch",
            "--audience",
            "user",
            "--storage",
            "local",
        ],
    );
    fixture.ok(
        &fixture.store,
        &["tracker", "track", "scratch", "notes.txt"],
    );
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join(".env.local"), "KEY=jack\n").expect("write");
    std::fs::write(workspace.join("notes.txt"), "private\n").expect("write");
    std::fs::write(workspace.join("wip.txt"), "uncommitted\n").expect("write");

    let output = fixture.ok(&workspace, &["checkpoint"]);
    assert!(!output.contains("not on the remote"), "{output}");

    // Source, uncommitted work included, is in the fork.
    let fork = fixture.remote.repo("store.i.feature-a");
    let tip = git(
        &fork,
        &["rev-parse", "refs/newgit/checkpoints/ckpt_001/source"],
    );
    assert_eq!(
        git(&fork, &["show", &format!("{tip}:wip.txt")]),
        "uncommitted"
    );

    // The remote tracker's rev is in jack's own repo for it; the local one is nowhere.
    let lane = fixture.remote.repo("store.t.runtime-env.jack");
    let lane_refs = git(
        &lane,
        &["for-each-ref", "--format=%(refname)", "refs/lanes/"],
    );
    assert!(
        lane_refs.starts_with("refs/lanes/runtime-env/revs/"),
        "{lane_refs}"
    );
    assert!(
        !temp.join("artifacts/store.t.scratch.jack.git").exists(),
        "local trackers stay local"
    );

    // The record was written last, naming those refs.
    let requests = fixture.remote.requests();
    let put = requests
        .iter()
        .position(|(m, p, _)| m == "PUT" && p.ends_with("/checkpoints/feature-a/ckpt_001"));
    let last_credential = requests
        .iter()
        .rposition(|(_, p, _)| p.ends_with("/credentials"));
    assert!(
        put.is_some() && put > last_credential,
        "record after the refs it names"
    );
}

/// Run newgit as a given user: their own HOME, so their own saved login.
fn newgit_as(home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .env("HOME", home.as_str())
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("newgit runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

fn save_login(home: &Utf8Path, url: &str, login: &str, token_id: &str) {
    std::fs::create_dir_all(home.join(".newgit")).expect("mkdir");
    std::fs::write(
        home.join(".newgit/auth.toml"),
        format!(
            "[remotes.\"{url}\"]\nlogin = \"{login}\"\ntoken = \"{}\"\ntoken_id = \"{token_id}\"\n",
            fake_token(token_id)
        ),
    )
    .expect("write auth");
}

const DB_RESOURCE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "echo jacks-rows > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "cp {{state_ref}} restored.sql"
"#;

#[test]
fn a_teammate_spawns_a_checkpoint_running_with_their_own_keys() {
    let (_guard, temp) = tempdir();
    let remote = FakeRemote::start(temp.join("artifacts"));
    std::fs::create_dir_all(temp.join("artifacts")).expect("mkdir");
    let (jack_home, ana_home) = (temp.join("home-jack"), temp.join("home-ana"));
    save_login(&jack_home, &remote.url, "jack", LOGIN_TOKEN_ID);
    save_login(&ana_home, &remote.url, "ana", ANA_TOKEN_ID);

    // Jack's machine: the project, its trackers and a database resource,
    // committed so a teammate's clone has the same definitions.
    let jack = temp.join("store");
    std::fs::create_dir_all(&jack).expect("mkdir");
    git(&jack, &["init", "-q", "-b", "main"]);
    std::fs::write(jack.join("README.md"), "hello\n").expect("write");
    git(&jack, &["add", "."]);
    git(&jack, &["commit", "-q", "-m", "initial"]);
    newgit_as(&jack_home, &jack, &["init"]);
    let config = jack.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    // Name the remote first, so trackers may be remote-stored; `remote add`
    // below creates the project and seeds trunk with all of this.
    text.push_str(&format!("\n[remote]\nurl = \"{}\"\n", remote.url));
    std::fs::write(&config, text).expect("write config");
    newgit_as(
        &jack_home,
        &jack,
        &[
            "tracker",
            "create",
            "runtime-env",
            "--audience",
            "user",
            "--storage",
            "remote",
        ],
    );
    newgit_as(
        &jack_home,
        &jack,
        &["tracker", "track", "runtime-env", ".env.local"],
    );
    newgit_as(
        &jack_home,
        &jack,
        &[
            "tracker",
            "create",
            "db-snapshots",
            "--audience",
            "project-devs",
            "--storage",
            "remote",
        ],
    );
    std::fs::write(jack.join(".newgit/resources/db.toml"), DB_RESOURCE).expect("resource");
    git(&jack, &["add", "-A"]);
    git(&jack, &["commit", "-q", "-m", "newgit definitions"]);
    newgit_as(&jack_home, &jack, &["remote", "add"]);

    // Jack's agent works: a commit, uncommitted work, Jack's key, a database.
    newgit_as(&jack_home, &jack, &["spawn", "feature-a"]);
    let jack_ws =
        Utf8PathBuf::from(newgit_as(&jack_home, &jack, &["status", "feature-a", "--path"]).trim());
    std::fs::write(jack_ws.join("feature.txt"), "committed work\n").expect("write");
    git(&jack_ws, &["add", "feature.txt"]);
    git(&jack_ws, &["commit", "-q", "-m", "feature"]);
    std::fs::write(jack_ws.join("wip.txt"), "uncommitted work\n").expect("write");
    std::fs::write(jack_ws.join(".env.local"), "STRIPE_KEY=jacks-key\n").expect("write");
    let checkpointed = newgit_as(&jack_home, &jack_ws, &["checkpoint"]);
    assert!(
        !checkpointed.contains("not on the remote"),
        "{checkpointed}"
    );

    // Ana's machine: a clone of trunk, her own key, never Jack's.
    let ana = temp.join("ana-store");
    git(
        &temp,
        &[
            "clone",
            "-q",
            remote.repo("store.src").as_str(),
            ana.as_str(),
        ],
    );
    // The clone already names the remote; joining checks Ana is a member.
    newgit_as(&ana_home, &ana, &["remote", "add"]);
    std::fs::write(ana.join(".env.local"), "STRIPE_KEY=anas-key\n").expect("write");
    newgit_as(
        &ana_home,
        &ana,
        &["tracker", "capture", "runtime-env", "--from-store"],
    );

    let spawned = newgit_as(
        &ana_home,
        &ana,
        &[
            "spawn",
            "review",
            "--from-checkpoint",
            "feature-a/ckpt_001",
            "--json",
        ],
    );
    let outcome: Value = serde_json::from_str(spawned.trim()).expect("json");
    assert_eq!(outcome["author"], "jack");
    let ana_ws = Utf8PathBuf::from(outcome["workspace"].as_str().expect("workspace"));

    let read = |file: &str| {
        std::fs::read_to_string(ana_ws.join(file)).unwrap_or_else(|_| panic!("{file} missing"))
    };
    assert_eq!(
        read("feature.txt"),
        "committed work\n",
        "source came across"
    );
    assert_eq!(
        read("wip.txt"),
        "uncommitted work\n",
        "uncommitted work came across, still uncommitted"
    );
    assert!(git(&ana_ws, &["status", "--porcelain"]).contains("wip.txt"));
    assert_eq!(
        read(".env.local"),
        "STRIPE_KEY=anas-key\n",
        "Ana runs it with her own key"
    );
    assert_eq!(
        read("restored.sql"),
        "jacks-rows\n",
        "the database came across from its dump"
    );

    // And Jack's key is nowhere on Ana's machine.
    let ana_lanes = ana.join(".newgit/local/lanes.git");
    let objects = git(
        &ana_lanes,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    for object in objects.lines() {
        assert!(!git(&ana_lanes, &["cat-file", "-p", object]).contains("jacks-key"));
    }
}
