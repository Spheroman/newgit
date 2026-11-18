//! #54: a runtime dependency. A resource whose `autostart` process depends
//! on another's starts once that one answers, not merely once its process
//! exists; one whose dependency never answers is not started against it,
//! and the next `newgit start` brings it up once the dependency does.
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
    let dir = std::env::temp_dir().join(format!("newgit-depwait-{}-{n}", std::process::id()));
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

/// A server that listens on its port only after `delay` seconds, like a dev
/// server compiling. Each test scans from its own `start`, so one test's
/// server, not listening yet, is never another's.
fn api(start: u16, delay: u32) -> String {
    format!(
        r#"ownership = "branch"

[ports]
api = {{ start = {start} }}

[exports]
API_PORT = "{{{{ports.api}}}}"

[actions.serve]
command = "sleep {delay}; exec python3 -m http.server $API_PORT --bind 127.0.0.1"
long_running = true
autostart = true
"#
    )
}

/// Depends on `api` and writes, the moment it starts, whether `api` was
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
open("api-at-start", "w").write(state)
'
exec sleep 600"""
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

fn api_at_start(workspace: &Utf8Path) -> Option<String> {
    // Started detached, so its file may land a moment after `spawn` returns.
    for _ in 0..50 {
        if let Ok(state) = std::fs::read_to_string(workspace.join("api-at-start")) {
            return Some(state);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

#[test]
fn a_dependent_starts_once_its_dependency_answers() {
    if !has_python() {
        return;
    }
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "api", &api(free_port(), 2));
    write_resource(&store, "web", WEB);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{spawned}");
    assert_eq!(started(&spawned, "web")["status"], "started", "{spawned}");
    let workspace = Utf8PathBuf::from(spawned["workspace"].as_str().expect("workspace"));
    assert_eq!(
        api_at_start(&workspace).as_deref(),
        Some("answering"),
        "`web` started before `api` answered"
    );
}

#[test]
fn a_dependency_that_never_answers_keeps_its_dependent_down_until_start() {
    if !has_python() {
        return;
    }
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    // Past the five seconds a dependent waits.
    write_resource(&store, "api", &api(free_port(), 7));
    write_resource(&store, "web", WEB);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(1), "{spawned}");
    let web = started(&spawned, "web");
    assert_eq!(web["status"], "failed", "{spawned}");
    let error = web["error"].as_str().expect("error");
    assert!(
        error.contains("depends on `api`") && error.contains("not answering"),
        "{error}"
    );
    assert!(error.contains("`newgit start`"), "{error}");
    let workspace = Utf8PathBuf::from(spawned["workspace"].as_str().expect("workspace"));
    assert!(!workspace.join("api-at-start").exists());

    // Once `api` answers, `start` owes `web` its start.
    let port =
        find_port(&spawned["resources"]).unwrap_or_else(|| panic!("no api port in {spawned}"));
    let answering = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert!(answering, "`api` never came up");
    let (code, start) = json(&repo, &["start", "x", "--json"]);
    assert_eq!(code, Some(0), "{start}");
    let resources = start["resources"].as_array().expect("resources");
    assert!(
        resources
            .iter()
            .any(|entry| entry["name"] == "web" && entry["status"] == "started"),
        "{start}"
    );
    assert_eq!(api_at_start(&workspace).as_deref(), Some("answering"));
}

fn find_port(value: &Value) -> Option<u16> {
    match value {
        Value::Object(map) => {
            if map.get("resource").and_then(Value::as_str) == Some("api")
                && let Some(port) = map.get("port").and_then(Value::as_u64)
            {
                return u16::try_from(port).ok();
            }
            if let Some(port) = map.get("api").and_then(Value::as_u64) {
                return u16::try_from(port).ok();
            }
            map.values().find_map(find_port)
        }
        Value::Array(items) => items.iter().find_map(find_port),
        _ => None,
    }
}

#[test]
fn starting_a_dependent_by_hand_refuses_while_its_dependency_is_down() {
    if !has_python() {
        return;
    }
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "api", &api(free_port(), 0));
    write_resource(&store, "web", WEB);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{spawned}");
    let (code, stdout, stderr) = newgit(&repo, &["stop", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    let (code, stdout, stderr) = newgit(&repo, &["action", "web.serve", "x"]);
    assert_ne!(code, Some(0), "{stdout}{stderr}");
    assert!(
        stderr.contains("depends on `api`, whose `serve` is not running"),
        "{stderr}"
    );
    assert!(stderr.contains("newgit action api.serve"), "{stderr}");

    // `start` brings both back, `api` first.
    let (code, start) = json(&repo, &["start", "x", "--json"]);
    assert_eq!(code, Some(0), "{start}");
    let names: Vec<&str> = start["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["api", "web"], "{start}");
}
