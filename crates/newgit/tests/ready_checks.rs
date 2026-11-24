//! #276: readiness a definition controls. `[ready]` names what "up" means
//! (a GET that answers 200-399, or a command that exits 0) and how long to
//! wait for it; spawn, `start`, and a dependent's start wait for that, and
//! `status` shows `starting` until it holds or that wait is over, when it is
//! a problem.
//!
//! These drive the binary, since what is under test is what it prints.

#![allow(dead_code)]

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::Value;

fn git(dir: &Utf8Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A directory under the OS temp dir, unique per call. On drop it stops
/// whatever the instance still runs, then removes the directory.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(self.0.join("store").as_str())
            .args(["stop", "x"])
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-ready-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn json(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, Value) {
    let (code, stdout, stderr) = newgit(repo, args);
    let value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("`newgit {args:?}` stdout is not one JSON document ({error}):\n{stdout}\n{stderr}")
    });
    (code, value)
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// Where newgit's port search starts: a port the OS just handed out, so
/// two runs at once do not search the same range.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("an OS-assigned port")
        .port()
}

/// A server in this test process that answers every request with `status`,
/// for as long as the test runs. Its port is the OS's choice. No Python to
/// start, so nothing to wait for: it answers before the spawn begins.
fn answering(status: u16) -> u16 {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Whatever\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
        }
    });
    port
}

/// Answers 503 until `delay` seconds after it started, then 200: a server
/// that binds early and serves errors while it compiles.
fn compiling_server(delay: u32, ready: &str) -> String {
    format!(
        r#"ownership = "branch"

[ports]
web = {{ start = {}, env = "WEB_PORT" }}

[actions.serve]
command = """python3 -c '
import http.server, os, socketserver, time
began = time.time()
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200 if time.time() - began > {delay} else 503)
        self.end_headers()
    def log_message(self, *args):
        pass
socketserver.TCPServer.allow_reuse_address = True
socketserver.ThreadingTCPServer(("127.0.0.1", int(os.environ["WEB_PORT"])), H).serve_forever()
'"""
long_running = true
autostart = true

{ready}
"#,
        free_port()
    )
}

#[test]
fn an_http_check_waits_past_a_server_that_answers_errors() {
    if !has_python() {
        return;
    }
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    // Past the default five seconds, inside its own timeout.
    write_resource(
        &store,
        "web",
        &compiling_server(
            6,
            "[ready]\nhttp = \"http://127.0.0.1:{{ports.web}}/\"\ntimeout_seconds = 20\n",
        ),
    );
    let repo = store.paths().project_root.clone();

    let began = std::time::Instant::now();
    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{spawned}");
    assert_eq!(spawned["starting"], serde_json::json!([]), "{spawned}");
    assert!(
        began.elapsed() >= std::time::Duration::from_secs(6),
        "spawn did not wait for the 200"
    );
    let (_, status, _) = newgit(&repo, &["status"]);
    assert!(!status.contains("still starting"), "{status}");
}

#[test]
fn a_failing_http_check_fails_spawn_and_says_what_it_answered() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let port = answering(503);
    write_resource(
        &store,
        "web",
        &format!(
            "ownership = \"branch\"\n\n[ready]\nhttp = \"http://127.0.0.1:{port}/health\"\ntimeout_seconds = 1\n"
        ),
    );
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    // Still failing once its wait is over is a failure.
    assert_eq!(code, Some(1), "{spawned}");
    assert_eq!(spawned["starting"], serde_json::json!([]), "{spawned}");
    let problem = spawned["problems"][0].as_str().expect("a problem");
    assert!(
        problem.contains(&format!(
            "`web` is not up after 1s (GET http://127.0.0.1:{port}/health answered 503)"
        )),
        "{problem}"
    );

    let (code, status, _) = newgit(&repo, &["status"]);
    assert_eq!(code, Some(0), "{status}");
    assert!(status.contains(" not ready "), "{status}");
    assert!(status.contains("/health answered 503"), "{status}");
}

/// No process newgit supervises: `prepare` brings something up in the
/// background, as `supabase start` leaves containers, and it is up a couple
/// of seconds later.
const STACK: &str = r#"ownership = "branch"

[actions.prepare]
command = "(sleep 2; touch stack-up) > /dev/null 2>&1 &"

[ready]
command = "test -f stack-up"
timeout_seconds = 10
"#;

/// Depends on `stack` and writes whether it was up the moment it started.
const FUNCTIONS: &str = r#"ownership = "branch"
depends_on = ["stack"]

[actions.serve]
command = "if test -f stack-up; then echo up; else echo down; fi > stack-at-start; exec sleep 600"
long_running = true
autostart = true
"#;

#[test]
fn a_dependent_waits_for_an_unsupervised_stack_to_pass_its_check() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "stack", STACK);
    write_resource(&store, "functions", FUNCTIONS);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{spawned}");
    let workspace = Utf8PathBuf::from(spawned["workspace"].as_str().expect("workspace"));
    let state = (0..50).find_map(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::read_to_string(workspace.join("stack-at-start")).ok()
    });
    assert_eq!(state.as_deref(), Some("up\n"), "{spawned}");
}

#[test]
fn an_unsupervised_resource_still_failing_its_check_after_its_timeout_fails_spawn() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "stack",
        "ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"true\"\n\n[ready]\ncommand = \"exit 3\"\ntimeout_seconds = 1\n",
    );
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    // With no process to watch, still starting and gone down look alike;
    // past the time its definition gives it, it is down.
    assert_eq!(code, Some(1), "{spawned}");
    assert_eq!(spawned["starting"], serde_json::json!([]), "{spawned}");
    let problem = spawned["problems"][0].as_str().expect("a problem");
    assert!(
        problem.contains("`stack` is not up after 1s (`exit 3` exited 3)")
            && problem.contains("newgit runs no process of it to watch")
            && problem.contains("`newgit action stack.prepare`"),
        "{problem}"
    );

    let (_, status, _) = newgit(&repo, &["status"]);
    assert!(status.contains(" not ready "), "{status}");
    assert!(status.contains("`exit 3` exited 3"), "{status}");
}

#[test]
fn a_ready_section_that_cannot_mean_anything_is_refused() {
    for (ready, says) in [
        (
            "http = \"http://127.0.0.1:1/\"\ncommand = \"true\"",
            "`http` or `command`, not both",
        ),
        ("timeout_seconds = 0", "between 1 and 3600"),
        (
            "http = \"127.0.0.1:1\"",
            "must be an `http://` or `https://` URL",
        ),
        ("port = 1", "unknown field"),
    ] {
        let (_guard, temp) = tempdir();
        let store = setup(&temp);
        write_resource(
            &store,
            "web",
            &format!("ownership = \"branch\"\n\n[ready]\n{ready}\n"),
        );
        let repo = store.paths().project_root.clone();
        let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
        assert_ne!(code, Some(0), "{ready}: {stdout}");
        assert!(stderr.contains(says), "{ready}: {stderr}");
    }
}
