//! Review of #54 and #276: what a held-back dependent leaves behind.
//! `status` names an `autostart` action that never started, a dependency
//! that did not come up is waited for once and not once per dependent, a
//! `[ready]` check is run for a dependency started by hand, the HTTP check
//! does not go through a proxy, and `undo` waits as spawn does.
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
    let dir = std::env::temp_dir().join(format!("newgit-heldback-{}-{n}", std::process::id()));
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
    // A proxy that refuses everything: a check of this machine must not go
    // through it.
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
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

/// Where newgit's port search starts: a port the OS just handed out, so
/// two runs at once do not search the same range (#276).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("an OS-assigned port")
        .port()
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// Runs, and its check never passes.
const NEVER_UP: &str = r#"ownership = "branch"

[actions.serve]
command = "exec sleep 600"
long_running = true
autostart = true

[ready]
command = "test -f never-there"
timeout_seconds = 1
"#;

const DEPENDENT: &str = r#"ownership = "branch"
depends_on = ["api"]

[actions.serve]
command = "exec sleep 600"
long_running = true
autostart = true
"#;

fn started<'a>(report: &'a Value, resource: &str) -> &'a Value {
    report["started"]
        .as_array()
        .expect("started array")
        .iter()
        .find(|entry| entry["name"] == resource)
        .unwrap_or_else(|| panic!("no `{resource}` in started: {report}"))
}

#[test]
fn status_names_a_dependent_that_was_held_back() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "api", NEVER_UP);
    write_resource(&store, "web", DEPENDENT);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(1), "{spawned}");
    assert_eq!(started(&spawned, "web")["status"], "failed", "{spawned}");

    let (_, stdout, stderr) = newgit(&repo, &["status", "x"]);
    let said = format!("{stdout}{stderr}");
    assert!(said.contains("`web.serve` was not started"), "{said}");
    assert!(said.contains("newgit start"), "{said}");
    assert!(said.contains("web:not started"), "{said}");
    assert!(said.contains("not ready"), "{said}");
}

#[test]
fn a_dependency_that_did_not_come_up_is_waited_for_once() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "api", NEVER_UP);
    for name in ["web-a", "web-b", "web-c"] {
        write_resource(&store, name, DEPENDENT);
    }
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(1), "{spawned}");
    let errors: Vec<&str> = ["web-a", "web-b", "web-c"]
        .iter()
        .map(|name| started(&spawned, name)["error"].as_str().expect("error"))
        .collect();
    assert_eq!(
        errors
            .iter()
            .filter(|error| error.contains("not up after 1s"))
            .count(),
        1,
        "{errors:?}"
    );
    assert_eq!(
        errors
            .iter()
            .filter(|error| error.contains("which did not come up"))
            .count(),
        2,
        "{errors:?}"
    );
}

#[test]
fn a_dependency_started_by_hand_is_still_checked() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    // Its process is the user's to start, and its check says it is not up.
    write_resource(&store, "api", &NEVER_UP.replace("autostart = true\n", ""));
    write_resource(&store, "web", DEPENDENT);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(1), "{spawned}");
    let web = started(&spawned, "web");
    assert_eq!(web["status"], "failed", "{spawned}");
    let error = web["error"].as_str().expect("error");
    assert!(
        error.contains("depends on `api`") && error.contains("not up after 1s"),
        "{error}"
    );
}

/// A server that listens only after `delay` seconds, up once it answers a
/// GET.
fn api(start: u16, delay: u32) -> String {
    format!(
        r#"ownership = "branch"

[ports]
api = {{ start = {start} }}

[exports]
API_PORT = "{{{{ports.api}}}}"

[actions.serve]
command = "sleep {delay}; exec python3 -c 'import http.server as h, socketserver as s, sys; s.ThreadingTCPServer.allow_reuse_address = True; s.ThreadingTCPServer((sys.argv[2], int(sys.argv[1])), h.SimpleHTTPRequestHandler).serve_forever()' $API_PORT 127.0.0.1"
long_running = true
autostart = true

[ready]
http = "http://127.0.0.1:{{{{ports.api}}}}/"
timeout_seconds = 30
"#
    )
}

/// Depends on `api` and writes, each time it starts, whether `api` was
/// answering then.
const WEB: &str = r#"ownership = "branch"
depends_on = ["api"]

[actions.serve]
command = """python3 -c '
import os, socket
try:
    socket.create_connection(("127.0.0.1", int(os.environ["API_PORT"])), 1).close()
    state = "answering"
except OSError:
    state = "silent"
open(os.environ["STARTS"], "a").write(state + "\\n")
'
exec sleep 600"""
long_running = true
autostart = true
"#;

fn starts(path: &Utf8Path, want: usize) -> Vec<String> {
    // Started detached, so a line may land a moment after the command returns.
    for _ in 0..100 {
        let lines: Vec<String> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        if lines.len() >= want {
            return lines;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Vec::new()
}

#[test]
fn an_http_check_is_not_asked_through_a_proxy_and_undo_waits_too() {
    if !has_python() {
        return;
    }
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let log = temp.join("starts");
    write_resource(&store, "api", &api(free_port(), 2));
    write_resource(
        &store,
        "web",
        &WEB.replace("os.environ[\"STARTS\"]", &format!("\"{log}\"")),
    );
    let repo = store.paths().project_root.clone();

    // `newgit` here runs under a proxy that refuses every connection.
    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{spawned}");
    assert_eq!(started(&spawned, "web")["status"], "started", "{spawned}");
    assert_eq!(starts(&log, 1), ["answering"]);

    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x", "-m", "up"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert_eq!(
        starts(&log, 2),
        ["answering", "answering"],
        "undo restarted `web` before `api` answered: {stdout}{stderr}"
    );
}
