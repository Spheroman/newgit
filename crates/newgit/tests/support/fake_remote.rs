// The fake newgit remote and the fixture around it, shared by the tests that
// talk to a remote: `include!`d, so each test binary has its own copy.

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
    /// The fake's own URL, for the endpoint it names.
    base: String,
    /// `(method, path, bearer token)` for every request, in order.
    requests: Vec<(String, String, String)>,
    /// Project key → branch name → list.
    branches: BTreeMap<String, BTreeMap<String, Value>>,
    /// Instance name → the code trackers it forked.
    forks: BTreeMap<String, Vec<String>>,
    checkpoints: BTreeMap<(String, String), Value>,
    /// Who checkpointed what, for `createdBy`.
    checkpoint_authors: BTreeMap<(String, String), String>,
    /// Instance token → the login it acts as.
    instance_tokens: BTreeMap<String, String>,
    /// Instances registered private.
    private: std::collections::BTreeSet<String>,
    /// Behave like a remote from before private instances.
    ignores_private: bool,
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
        let state = Arc::new(Mutex::new(State {
            base: url.clone(),
            ..State::default()
        }));
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
    let instance_json = |state: &State, owner: &str, name: &str| {
        json!({
            "name": name, "owner": owner, "agent": null, "baseRev": "x", "headRev": null,
            "forks": state.forks.get(name).cloned().unwrap_or_default(),
            "private": state.private.contains(name),
        })
    };
    match (method, segments.as_slice()) {
        ("GET", ["v1", "me"]) => (
            200,
            json!({ "login": caller, "kind": "login", "token_id": "tok" }),
        ),
        ("POST", ["v1", "projects"]) => {
            let name = body["name"].as_str().unwrap_or_default();
            let key = format!("{caller}/{name}");
            if state.branches.contains_key(&key) {
                return (
                    409,
                    json!({ "error": format!("project `{key}` already exists") }),
                );
            }
            state.branches.insert(key.clone(), BTreeMap::new());
            let mut seeds = Vec::new();
            for tracker in body["trackers"].as_array().cloned().unwrap_or_default() {
                let tracker = tracker.as_str().unwrap_or_default();
                let repo = format!("{caller}.{name}.t.{tracker}");
                git(
                    repos,
                    &["init", "-q", "--bare", "-b", "main", &repo_url(&repo)],
                );
                let mut seed = credential(&repo);
                seed["tracker"] = json!(tracker);
                seeds.push(seed);
            }
            (
                201,
                json!({
                    "name": name, "owner": caller, "key": key,
                    "endpoint": format!("{}/{key}.git", state.base), "seeds": seeds,
                }),
            )
        }
        ("GET", ["v1", "projects", owner, name]) => {
            let key = format!("{owner}/{name}");
            let Some(branches) = state.branches.get(&key) else {
                return (404, json!({ "error": format!("no project `{key}`") }));
            };
            let branches: Vec<Value> = branches
                .iter()
                .map(|(branch, list)| json!({ "name": branch, "list": list }))
                .collect();
            (
                200,
                json!({
                    "name": name, "owner": owner, "key": key, "defaultBranch": "main",
                    "endpoint": format!("{}/{key}.git", state.base),
                    "members": [{ "login": "ana" }, { "login": "jack" }], "branches": branches,
                }),
            )
        }
        ("DELETE", ["v1", "projects", owner, name]) => {
            let key = format!("{owner}/{name}");
            if state.branches.remove(&key).is_none() {
                return (404, json!({ "error": format!("no project `{key}`") }));
            }
            let prefix = format!("{owner}.{name}.");
            let mut deleted = 0;
            for entry in std::fs::read_dir(repos).expect("repos").flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    std::fs::remove_dir_all(entry.path()).expect("delete repo");
                    deleted += 1;
                }
            }
            (200, json!({ "key": key, "deleted": true, "repos_deleted": deleted }))
        }
        ("POST", ["v1", "projects", owner, name, "trackers"]) => {
            let tracker = body["name"].as_str().unwrap_or_default();
            let repo = format!("{owner}.{name}.t.{tracker}");
            if exists(&repo) {
                return (409, json!({ "error": format!("{repo} exists already") }));
            }
            git(
                repos,
                &["init", "-q", "--bare", "-b", "main", &repo_url(&repo)],
            );
            let mut seed = credential(&repo);
            seed["tracker"] = json!(tracker);
            (201, seed)
        }
        ("POST", ["v1", "projects", owner, name, "branches", branch, "merge"]) => {
            let key = format!("{owner}/{name}");
            if state
                .branches
                .get(&key)
                .and_then(|branches| branches.get(*branch))
                != Some(&body["base"])
            {
                return (
                    409,
                    json!({ "error": "the branch moved since you looked: merge again" }),
                );
            }
            let credentials: Vec<Value> = body["trackers"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|tracker| {
                    let tracker = tracker.as_str().unwrap_or_default();
                    let mut seed = credential(&format!("{owner}.{name}.t.{tracker}"));
                    seed["tracker"] = json!(tracker);
                    seed
                })
                .collect();
            (
                200,
                json!({ "branch": branch, "until": "later", "credentials": credentials }),
            )
        }
        ("PUT", ["v1", "projects", owner, name, "branches", branch]) => {
            let key = format!("{owner}/{name}");
            let branches = state.branches.entry(key).or_default();
            if !body["base"].is_null() {
                if branches.get(*branch) != Some(&body["base"]) {
                    return (
                        409,
                        json!({ "error": "the branch moved since you looked: merge again" }),
                    );
                }
                branches.insert((*branch).to_owned(), body["list"].clone());
                return (200, json!({ "name": branch, "list": body["list"] }));
            }
            if branches.contains_key(*branch) {
                return (
                    409,
                    json!({ "error": "branches move only through `newgit merge`" }),
                );
            }
            branches.insert((*branch).to_owned(), body["list"].clone());
            (201, json!({ "name": branch, "list": body["list"] }))
        }
        ("POST", ["v1", "projects", owner, project, "instances"]) => {
            let name = body["name"].as_str().unwrap_or_default();
            if body["private"].as_bool() == Some(true) && !state.ignores_private {
                state.private.insert(name.to_owned());
            }
            let instance_token = fake_token(&format!("tok_instance_{name}"));
            state
                .instance_tokens
                .insert(instance_token.clone(), caller.clone());
            let mut created = instance_json(&state, &caller, name);
            created["endpoint"] = json!(format!("{}/{owner}/{project}.git", state.base));
            created["token"] = json!(instance_token);
            created["token_id"] = json!(format!("tok_instance_{name}"));
            (201, created)
        }
        (
            "POST",
            [
                "v1",
                "projects",
                owner,
                project,
                "instances",
                name,
                "trackers",
                tracker,
                "fork",
            ],
        ) => {
            let fork = format!("{owner}.{project}.i.{name}.{tracker}");
            if !exists(&fork) {
                git(
                    repos,
                    &[
                        "clone",
                        "-q",
                        "--bare",
                        &repo_url(&format!("{owner}.{project}.t.{tracker}")),
                        &repo_url(&fork),
                    ],
                );
            }
            let forks = state.forks.entry((*name).to_owned()).or_default();
            if !forks.iter().any(|known| known == tracker) {
                forks.push((*tracker).to_owned());
            }
            (200, json!({ "repo": fork, "remote": repo_url(&fork) }))
        }
        ("GET", ["v1", "projects", _, _, "instances", name]) => {
            (200, instance_json(&state, &caller, name))
        }
        ("DELETE", ["v1", "projects", owner, project, "instances", name]) => {
            for tracker in state.forks.remove(*name).unwrap_or_default() {
                let _ = std::fs::remove_dir_all(repo_url(&format!(
                    "{owner}.{project}.i.{name}.{tracker}"
                )));
            }
            (200, json!({ "name": name, "removed": true }))
        }
        ("POST", ["v1", "projects", _, _, "credentials"]) => {
            let repo = body["repo"].as_str().unwrap_or_default();
            // Like the real remote: a per-user tracker's repo is its owner's alone.
            let parts: Vec<&str> = repo.split('.').collect();
            if parts.len() == 5 && parts[2] == "t" && parts[4] != caller {
                return (
                    403,
                    json!({ "error": format!("{repo} is {}'s own; each user reads only their own", parts[4]) }),
                );
            }
            if !exists(repo) {
                // A secret tracker's repo is created on first write; code
                // tracker repos are seeded and forks made by the fork route.
                if parts.get(2) == Some(&"t") && body["scope"] == "write" {
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
        ("PUT", ["v1", "projects", _, _, "instances", name, "head"]) => {
            (200, json!({ "name": name, "headRev": body["head_rev"] }))
        }
        ("PUT", ["v1", "projects", _, _, "checkpoints", instance, id]) => {
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
        ("GET", ["v1", "projects", _, _, "checkpoints", instance, id]) => {
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
    // The definitions are committed: the remote reads audiences from them.
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "newgit"]);
    fixture.ok(
        &fixture.store,
        &["remote", "add", "--url", &fixture.remote.url],
    );
    fixture
}

/// A tracker repo's (or fork's) commit, and the files in its tree.
fn files_at(repo: &Utf8Path, reference: &str) -> String {
    git(repo, &["ls-tree", "-r", "--name-only", reference])
}

fn split_source(repo: &Utf8Path, reference: &str) -> String {
    git(
        repo,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Newgit-Split,valueonly)",
            reference,
        ],
    )
}

fn workspace_of(fixture: &Fixture, name: &str) -> Utf8PathBuf {
    Utf8PathBuf::from(
        fixture
            .ok(&fixture.store, &["status", name, "--path"])
            .trim(),
    )
}

