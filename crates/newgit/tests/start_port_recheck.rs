//! What `newgit start`'s port check must and must not do (#441), beyond
//! `start_busy_port.rs`: leave alone a port only newgit's own stopped server
//! used, probe a resource that comes back through `resume` and do it before
//! the `resume` runs, and restart a running dependent whose environment
//! named the port that moved.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::time::{Duration, Instant};

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

/// Stops what the instance runs when the test ends, even on a failed
/// assertion, then removes the directory.
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
    let dir = std::env::temp_dir().join(format!("newgit-port-recheck-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join(".env"), "API=http://127.0.0.1:1\n").expect("write");
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

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    Run {
        status: output.status,
        stdout: String::from_utf8(output.stdout).expect("utf8 stdout"),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn newgit_ok(repo: &Utf8Path, args: &[&str]) -> Run {
    let run = newgit(repo, args);
    assert!(
        run.status.success(),
        "newgit {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    run
}

/// The port `x`'s `db` is bound to, from the record.
fn db_port(store: &MetadataStore) -> u16 {
    store.find_branch("x").expect("instance").resources["db"].resolved_ports["api"]
}

/// Whether newgit's supervisor has a live process group for `resource`.
fn running(store: &MetadataStore, resource: &str) -> bool {
    pid(store, resource).is_some_and(|pid| {
        Command::new("kill")
            .args(["-0", "--", &format!("-{pid}")])
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn pid(store: &MetadataStore, resource: &str) -> Option<u32> {
    std::fs::read_to_string(
        store
            .paths()
            .state
            .join("x")
            .join(format!("{resource}.pid")),
    )
    .ok()
    .and_then(|text| text.trim().parse().ok())
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A real server on its claimed port, which serves one request and is then
/// stopped by newgit. Its side of that connection is in TIME_WAIT when the
/// next `start` probes, and nothing outside newgit holds the port.
fn web(start: u16) -> String {
    format!(
        r#"ownership = "branch"

[ports.api]
start = {start}

[exports]
API_URL = "http://127.0.0.1:{{{{ports.api}}}}"

[actions.start]
command = "exec python3 -c 'import http.server as h, socketserver as s, sys; s.ThreadingTCPServer.allow_reuse_address = True; s.ThreadingTCPServer((sys.argv[2], int(sys.argv[1])), h.SimpleHTTPRequestHandler).serve_forever()' {{{{ports.api}}}} 127.0.0.1"
long_running = true
"#
    )
}

#[test]
fn stop_then_start_after_one_request_keeps_the_port() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    std::fs::write(store.paths().resources.join("db.toml"), web(46100)).expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "db.start", "x"]);
    let claimed = db_port(&store);
    // Wait for it to listen, then one request (HTTP/1.0: the server closes).
    let mut served = Vec::new();
    wait_until("the server answers", || {
        let Ok(mut stream) = TcpStream::connect(("127.0.0.1", claimed)) else {
            return false;
        };
        stream
            .write_all(b"GET / HTTP/1.0\r\n\r\n")
            .expect("request");
        let _ = stream.read_to_end(&mut served);
        true
    });
    assert!(!served.is_empty(), "the server answered");
    newgit_ok(&repo, &["stop", "x"]);

    let run = newgit_ok(&repo, &["start", "x", "--json"]);
    let json: Value = serde_json::from_str(&run.stdout).expect("one JSON document");
    assert!(json.get("moved_ports").is_none(), "{json}");
    assert_eq!(
        db_port(&store),
        claimed,
        "start moved a port only newgit's own stopped server had used"
    );
}

/// A stack `resume` brings up and `suspend` takes down, like the supabase
/// template: a Python server standing in for its containers, on the claimed
/// port. `resume` also writes down the port it was given.
fn stack(start: u16, long_running: bool) -> String {
    let process = if long_running {
        "\n[actions.watch]\ncommand = \"sleep 1000\"\nlong_running = true\nautostart = true\n"
    } else {
        ""
    };
    format!(
        r#"ownership = "branch"

[ports.api]
start = {start}

[exports]
API_URL = "http://127.0.0.1:{{{{ports.api}}}}"

[actions.resume]
command = "echo {{{{ports.api}}}} > resumed-on; python3 -c 'import http.server as h, socketserver as s, sys; s.ThreadingTCPServer.allow_reuse_address = True; s.ThreadingTCPServer((sys.argv[2], int(sys.argv[1])), h.SimpleHTTPRequestHandler).serve_forever()' {{{{ports.api}}}} 127.0.0.1 </dev/null >/dev/null 2>&1 & echo $! > stack.pid; for i in $(seq 100); do python3 -c 'import socket,sys; socket.create_connection((\"127.0.0.1\", {{{{ports.api}}}}))' 2>/dev/null && exit 0; sleep 0.1; done; exit 1"

[actions.suspend]
command = "if [ -f stack.pid ]; then kill $(cat stack.pid); rm stack.pid; fi"
{process}"#
    )
}

/// Takes down what a `resume` left running when the test ends, `newgit
/// stop` or not.
struct Stack(Utf8PathBuf);

impl Drop for Stack {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.0.join("stack.pid")) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
    }
}

fn spawned_stack(start: u16, long_running: bool) -> (TempDir, Utf8PathBuf, MetadataStore, Stack) {
    let (guard, temp) = tempdir();
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("db.toml"),
        stack(start, long_running),
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let workspace = store.find_branch("x").expect("instance").workspace_path;
    (guard, repo, store, Stack(workspace))
}

/// The port is probed before the `resume`, not between it and the process:
/// the server `resume` brings up on the claimed port is the resource's own.
#[test]
fn what_resume_brings_up_on_the_port_is_not_another_program() {
    let (_guard, repo, store, stack) = spawned_stack(46120, true);
    let claimed = db_port(&store);
    newgit_ok(&repo, &["stop", "x"]);

    let run = newgit_ok(&repo, &["start", "x", "--json"]);
    let json: Value = serde_json::from_str(&run.stdout).expect("one JSON document");
    assert!(json.get("moved_ports").is_none(), "{json}");
    assert_eq!(db_port(&store), claimed, "{json}");
    assert_eq!(
        std::fs::read_to_string(stack.0.join("resumed-on")).expect("resume ran"),
        format!("{claimed}\n")
    );
}

/// A resource with no long-running action, that only `resume` brings back,
/// still has its ports checked, and `resume` is given the new one.
#[test]
fn a_resource_brought_back_by_resume_is_probed() {
    let (_guard, repo, store, stack) = spawned_stack(46140, false);
    let claimed = db_port(&store);
    newgit_ok(&repo, &["stop", "x"]);
    let _held = TcpListener::bind(("0.0.0.0", claimed)).expect("another program binds the port");

    let run = newgit_ok(&repo, &["start", "x", "--json"]);
    let json: Value = serde_json::from_str(&run.stdout).expect("one JSON document");
    let moved = db_port(&store);
    assert_ne!(moved, claimed, "{json}");
    assert_eq!(
        json["moved_ports"],
        serde_json::json!([{"resource": "db", "port": "api", "from": claimed, "to": moved}])
    );
    assert_eq!(
        std::fs::read_to_string(stack.0.join("resumed-on")).expect("resume ran"),
        format!("{moved}\n")
    );
}

/// `db` claims a port and exports it; `app` depends on it and writes down the
/// `API_URL` it was started with, each time.
fn db(start: u16) -> String {
    format!(
        r#"ownership = "branch"

[ports.api]
start = {start}
env = "DB_PORT"

[exports]
API_URL = "http://127.0.0.1:{{{{ports.api}}}}"

# Listens, since `app` starts only once `db` answers (#54).
[actions.start]
command = "exec python3 -c 'import http.server as h, socketserver as s, sys; s.ThreadingTCPServer.allow_reuse_address = True; s.ThreadingTCPServer((sys.argv[2], int(sys.argv[1])), h.SimpleHTTPRequestHandler).serve_forever()' $DB_PORT 127.0.0.1"
long_running = true
autostart = true
"#
    )
}

const APP: &str = r#"ownership = "branch"
depends_on = ["db"]

[actions.serve]
command = "echo \"$API_URL\" >> seen; exec sleep 1000"
long_running = true
autostart = true
"#;

/// #261 with a move: `db` dies on its own while `app` runs on, and another
/// program takes `db`'s port. `start` brings `db` back on a free port, and
/// restarts `app`, whose environment named the old one, which is now that
/// other program's.
#[test]
fn a_running_dependent_is_restarted_with_the_new_port() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    std::fs::write(store.paths().resources.join("db.toml"), db(46160)).expect("write db");
    std::fs::write(store.paths().resources.join("app.toml"), APP).expect("write app");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let workspace = store.find_branch("x").expect("instance").workspace_path;
    let claimed = db_port(&store);
    let seen = workspace.join("seen");
    wait_until("app is up", || seen.is_file());
    let app_before = pid(&store, "app").expect("app runs");

    // `db` dies on its own; something else takes its port.
    let db_pid = pid(&store, "db").expect("db runs");
    Command::new("kill")
        .args(["-KILL", "--", &format!("-{db_pid}")])
        .status()
        .expect("kill");
    wait_until("db is gone", || !running(&store, "db"));
    let _held = TcpListener::bind(("0.0.0.0", claimed)).expect("another program binds the port");

    let run = newgit_ok(&repo, &["start", "x", "--json"]);
    let json: Value = serde_json::from_str(&run.stdout).expect("one JSON document");
    let moved = db_port(&store);
    assert_ne!(moved, claimed, "{json}");
    assert_eq!(
        json["resources"],
        serde_json::json!([
            {"name": "db", "action": "start", "status": "started"},
            {"name": "app", "action": "serve", "status": "started"},
        ]),
        "{json}"
    );
    let warnings = json["warnings"].to_string();
    assert!(
        warnings.contains("`app` depends on `db`") && warnings.contains("restarted"),
        "{json}"
    );

    assert!(running(&store, "app"), "app is back");
    assert_ne!(pid(&store, "app"), Some(app_before), "app was restarted");
    wait_until("app is started again", || {
        std::fs::read_to_string(&seen).is_ok_and(|text| text.lines().count() == 2)
    });
    assert_eq!(
        std::fs::read_to_string(&seen).expect("seen"),
        format!("http://127.0.0.1:{claimed}\nhttp://127.0.0.1:{moved}\n")
    );
}
