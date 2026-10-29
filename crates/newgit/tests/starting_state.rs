//! #243: a dev server that is still compiling has not failed. A running
//! process whose port is not answering yet is `starting` -- not a problem in
//! `spawn`'s verdict, not "not ready" in `status` -- and `spawn` waits a few
//! seconds for it before saying so.
//!
//! These drive the binary, since what is under test is what it prints.

#![allow(dead_code)]

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::{Value, json};

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
/// whatever the instance still runs, so a failed assertion does not leave
/// `sleep`s behind, then removes the directory.
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
    let dir = std::env::temp_dir().join(format!("newgit-starting-{}-{n}", std::process::id()));
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

/// `newgit <verb> x --json`: its exit code and its stdout parsed as one JSON
/// document — which is itself an assertion: nothing may be printed beside it.
fn json_run(repo: &Utf8Path, verb: &str) -> (Option<i32>, Value) {
    let run = newgit(repo, &[verb, "x", "--json"]);
    let json = serde_json::from_str(&run.stdout).unwrap_or_else(|error| {
        panic!(
            "`newgit {verb} x --json` stdout is not one JSON document ({error}):\n{}\nstderr:\n{}",
            run.stdout, run.stderr
        )
    });
    (run.status.code(), json)
}

/// Listens on `web` only after `delay` seconds, like a server compiling.
fn server(delay: u32) -> String {
    format!(
        r#"ownership = "branch"

[ports]
web = {{ start = 54700 }}

[exports]
WEB_PORT = "{{{{ports.web}}}}"

[actions.serve]
command = "sleep {delay}; exec python3 -m http.server $WEB_PORT --bind 127.0.0.1"
long_running = true
autostart = true
"#
    )
}

/// Never listens at all within a test's lifetime.
const SILENT: &str = r#"ownership = "branch"

[ports]
web = { start = 54750 }

[actions.serve]
command = "sleep 1000"
long_running = true
autostart = true
"#;

#[test]
fn a_port_that_is_not_answering_yet_is_starting_not_a_failure() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", SILENT);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json_run(&repo, "spawn");
    assert_eq!(code, Some(0), "{spawned}");
    let starting = spawned["starting"].as_array().expect("starting array");
    assert_eq!(starting.len(), 1, "{spawned}");
    assert_eq!(starting[0]["resource"], "app");
    assert!(starting[0]["ports"]["web"].is_u64(), "{spawned}");

    let status = newgit(&repo, &["status"]);
    assert!(status.status.success(), "{}", status.stderr);
    assert!(status.stdout.contains(" starting "), "{}", status.stdout);
    assert!(!status.stdout.contains("not ready"), "{}", status.stdout);
    assert!(
        status.stdout.contains("`x` is still starting")
            && status.stdout.contains("not answering yet"),
        "{}",
        status.stdout
    );
}

#[test]
fn the_text_spawn_says_it_is_starting_and_where_to_look() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", SILENT);
    let repo = store.paths().project_root.clone();

    let run = newgit(&repo, &["spawn", "x"]);
    assert!(run.status.success(), "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("starting: `x`"), "{}", run.stdout);
    assert!(run.stdout.contains("still starting"), "{}", run.stdout);
    assert!(run.stdout.contains("`newgit status`"), "{}", run.stdout);
    assert!(!run.stdout.contains("not ready"), "{}", run.stdout);
}

#[test]
fn spawn_waits_for_a_server_that_answers_within_the_wait() {
    if Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", &server(1));
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json_run(&repo, "spawn");
    assert_eq!(code, Some(0), "{spawned}");
    assert_eq!(spawned["starting"], json!([]), "{spawned}");

    let status = newgit(&repo, &["status"]);
    assert!(status.stdout.contains("ready"), "{}", status.stdout);
    assert!(
        !status.stdout.contains("still starting"),
        "{}",
        status.stdout
    );
}
