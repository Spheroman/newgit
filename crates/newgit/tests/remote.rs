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
}

struct FakeRemote {
    url: String,
    repos: Utf8PathBuf,
    state: Arc<Mutex<State>>,
}

const LOGIN_TOKEN_ID: &str = "tok_login";
const INSTANCE_TOKEN_ID: &str = "tok_instance";

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
    let repo_url = |name: &str| repos.join(format!("{name}.git")).to_string();
    let segments: Vec<&str> = path.trim_start_matches('/').split(['/', '?']).collect();
    let credential = |repo: &str| json!({ "repo": repo, "remote": repo_url(repo), "username": "x", "password": "pw", "expires_at": "2099-01-01T00:00:00Z" });
    match (method, segments.as_slice()) {
        ("GET", ["v1", "me"]) => (
            200,
            json!({ "login": "jack", "kind": "login", "token_id": LOGIN_TOKEN_ID }),
        ),
        ("POST", ["v1", "projects"]) => {
            let name = body["name"].as_str().unwrap_or_default();
            let src = format!("{name}.src");
            git(
                repos,
                &["init", "-q", "--bare", "-b", "main", &repo_url(&src)],
            );
            (
                201,
                json!({ "name": name, "owner": "jack", "src": { "repo": src, "remote": repo_url(&src) }, "seed_credential": credential(&src) }),
            )
        }
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
            (
                201,
                json!({
                    "name": name, "owner": "jack", "agent": null, "baseRev": body["base_rev"], "headRev": null,
                    "forkState": "pending", "repo": fork, "remote": repo_url(&fork),
                    "trunk": { "repo": format!("{project}.src"), "remote": repo_url(&format!("{project}.src")) },
                    "token": fake_token(INSTANCE_TOKEN_ID), "token_id": INSTANCE_TOKEN_ID,
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
                json!({ "name": name, "owner": "jack", "baseRev": "x", "forkState": fork_state, "repo": fork, "remote": repo_url(&fork) }),
            )
        }
        ("DELETE", ["v1", "projects", project, "instances", name]) => {
            let _ = std::fs::remove_dir_all(repo_url(&format!("{project}.i.{name}")));
            (200, json!({ "name": name, "removed": true }))
        }
        ("POST", ["v1", "projects", _, "credentials"]) => {
            let repo = body["repo"].as_str().unwrap_or_default();
            (200, credential(repo))
        }
        ("PUT", ["v1", "projects", _, "checkpoints", instance, id]) => {
            state.checkpoints.insert(
                ((*instance).to_owned(), (*id).to_owned()),
                body["record"].clone(),
            );
            (
                201,
                json!({ "instance": instance, "id": id, "createdBy": "jack", "createdAt": "now", "record": body["record"] }),
            )
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
        fake_token(INSTANCE_TOKEN_ID)
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

    let instance_token = fake_token(INSTANCE_TOKEN_ID);
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
