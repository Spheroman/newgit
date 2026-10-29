//! `autostart` on a long-running action: `newgit spawn` starts it once
//! every `prepare` has run, dependencies first, and says when it could not.
//! T3 Code and Claude Code spawn with `--json` and go by the exit code, so
//! these drive the binary the same way.

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
    let dir = std::env::temp_dir().join(format!("newgit-autostart-{}-{n}", std::process::id()));
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

/// Whether newgit's supervisor has a live process for `resource` of `x`.
fn running(store: &MetadataStore, resource: &str) -> bool {
    let pid_file = store
        .paths()
        .state
        .join("x")
        .join(format!("{resource}.pid"));
    let Some(pid) = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
    else {
        return false;
    };
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

const DB: &str = r#"ownership = "branch"

[actions.start]
command = "sleep 1000"
long_running = true
autostart = true
"#;

const WEB: &str = r#"ownership = "branch"
depends_on = ["db"]

[actions.serve]
command = "sleep 1000"
long_running = true
autostart = true
"#;

/// Startable, but only by hand.
const IDLE: &str = r#"ownership = "branch"

[actions.start]
command = "sleep 1000"
long_running = true
"#;

#[test]
fn spawn_starts_autostart_actions_dependencies_first() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "web", WEB);
    write_resource(&store, "db", DB);
    write_resource(&store, "idle", IDLE);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json_run(&repo, "spawn");
    assert_eq!(code, Some(0), "{spawned}");
    assert_eq!(
        spawned["started"],
        json!([
            {"name": "db", "action": "start", "status": "started"},
            {"name": "web", "action": "serve", "status": "started"},
        ])
    );
    assert!(running(&store, "db") && running(&store, "web"));
    assert!(!running(&store, "idle"), "`idle` has no autostart");

    // What spawn started is what `stop` takes down and `start` brings back.
    let (code, stopped) = json_run(&repo, "stop");
    assert_eq!(code, Some(0), "{stopped}");
    assert_eq!(stopped["resources"].as_array().map(Vec::len), Some(2));
    assert!(!running(&store, "db") && !running(&store, "web"));
}

#[test]
fn a_failed_prepare_keeps_its_process_and_its_dependents_down() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "db",
        &format!("{DB}\n[actions.prepare]\ncommand = \"false\"\n"),
    );
    write_resource(&store, "web", WEB);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json_run(&repo, "spawn");
    assert_eq!(
        code,
        Some(1),
        "an instance with nothing running is not up: {spawned}"
    );
    let started = spawned["started"].as_array().expect("started is a list");
    assert_eq!(started.len(), 2, "{spawned}");
    assert_eq!(started[0]["name"], "db");
    assert_eq!(started[0]["status"], "failed");
    assert!(
        started[0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("failed")),
        "{spawned}"
    );
    assert_eq!(started[1]["name"], "web");
    assert_eq!(started[1]["status"], "failed");
    assert!(
        started[1]["error"]
            .as_str()
            .is_some_and(|error| error.contains("`db`")),
        "the dependent names what kept it down: {spawned}"
    );
    assert!(!running(&store, "db") && !running(&store, "web"));
}

#[test]
fn the_plain_spawn_says_what_it_started() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB);
    let repo = store.paths().project_root.clone();

    let run = newgit(&repo, &["spawn", "x"]);
    assert!(run.status.success(), "{}\n{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("started:   `db.start`"),
        "{}",
        run.stdout
    );
    assert!(running(&store, "db"));
}

#[test]
fn autostart_needs_a_long_running_action() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "db",
        "ownership = \"branch\"\n\n[actions.start]\ncommand = \"true\"\nautostart = true\n",
    );
    let repo = store.paths().project_root.clone();

    let run = newgit(&repo, &["spawn", "x"]);
    assert!(!run.status.success(), "{}", run.stdout);
    assert!(
        run.stderr.contains("has autostart but is not long_running"),
        "{}",
        run.stderr
    );
}

#[test]
fn only_one_action_per_resource_may_autostart() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "db",
        &format!(
            "{DB}\n[actions.other]\ncommand = \"sleep 1000\"\nlong_running = true\nautostart = true\n"
        ),
    );
    let repo = store.paths().project_root.clone();

    let run = newgit(&repo, &["spawn", "x"]);
    assert!(!run.status.success(), "{}", run.stdout);
    assert!(run.stderr.contains("all have autostart"), "{}", run.stderr);
}

#[test]
fn using_a_tracker_again_starts_what_it_brings() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB);
    std::fs::write(
        store.paths().trackers.join("local-db.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".db-env\"]\nresources = [\"db\"]\n",
    )
    .expect("write tracker");
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json_run(&repo, "spawn");
    assert_eq!(code, Some(0), "{spawned}");
    assert!(running(&store, "db"));

    let run = newgit(&repo, &["tracker", "drop", "local-db", "x"]);
    assert!(run.status.success(), "{}\n{}", run.stdout, run.stderr);
    assert!(
        !running(&store, "db"),
        "a drop stops what the tracker brings"
    );

    let run = newgit(&repo, &["tracker", "use", "local-db", "x"]);
    assert!(run.status.success(), "{}\n{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("started:   `db.start`"),
        "{}",
        run.stdout
    );
    assert!(running(&store, "db"), "and using it again starts them");
}
