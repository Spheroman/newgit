//! `newgit stop` / `newgit start` (#157): stop every running long-running
//! action of an instance, dependents first, and bring back exactly what was
//! stopped. T3 Code calls them on thread settle and unsettle, from the
//! project root, with the instance named and `--json`, so these drive the
//! binary the same way.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

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
    let dir = std::env::temp_dir().join(format!("newgit-stop-start-{}-{n}", std::process::id()));
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

fn newgit_ok(repo: &Utf8Path, args: &[&str]) {
    let run = newgit(repo, args);
    assert!(
        run.status.success(),
        "newgit {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
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
"#;

const WEB: &str = r#"ownership = "branch"
depends_on = ["db"]

[actions.serve]
command = "sleep 1000"
long_running = true
"#;

#[test]
fn stop_then_start_round_trips_only_what_was_running() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB);
    write_resource(&store, "web", WEB);
    // Defined and startable, but nobody starts it: it must stay stopped.
    write_resource(&store, "idle", DB);
    let repo = store.paths().project_root.clone();

    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "db.start", "x"]);
    newgit_ok(&repo, &["action", "web.serve", "x"]);
    assert!(running(&store, "db") && running(&store, "web"));

    // Dependents first: `web` depends on `db`.
    let (code, stopped) = json_run(&repo, "stop");
    assert_eq!(code, Some(0), "{stopped}");
    assert_eq!(
        stopped,
        json!({
            "instance": "x",
            "resources": [
                {"name": "web", "action": "serve", "status": "stopped"},
                {"name": "db", "action": "start", "status": "stopped"},
            ],
        })
    );
    assert!(!running(&store, "db") && !running(&store, "web"));

    // Idempotent: nothing is running, so nothing to do, and still exit 0.
    let (code, again) = json_run(&repo, "stop");
    assert_eq!(code, Some(0), "{again}");
    assert_eq!(again, json!({"instance": "x", "resources": []}));

    // Dependencies first, and only what the stop stopped.
    let (code, started) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(
        started,
        json!({
            "instance": "x",
            "resources": [
                {"name": "db", "action": "start", "status": "started"},
                {"name": "web", "action": "serve", "status": "started"},
            ],
        })
    );
    assert!(running(&store, "db") && running(&store, "web"));
    assert!(!running(&store, "idle"), "nobody had started `idle`");

    // The record is cleared: a second start has nothing to do.
    let (code, again) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{again}");
    assert_eq!(again, json!({"instance": "x", "resources": []}));
}

#[test]
fn start_with_nothing_stopped_does_nothing() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);

    let (code, started) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(started, json!({"instance": "x", "resources": []}));
    assert!(!running(&store, "db"));

    let plain = newgit(&repo, &["start", "x"]);
    assert!(plain.status.success(), "{}", plain.stderr);
}

#[test]
fn a_second_stop_adds_to_what_the_first_recorded() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB);
    write_resource(&store, "web", WEB);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "db.start", "x"]);
    newgit_ok(&repo, &["action", "web.serve", "x"]);

    newgit_ok(&repo, &["stop", "x"]);
    // Started by hand in between, then stopped again: `start` still owes
    // `web` from the first stop.
    newgit_ok(&repo, &["action", "db.start", "x"]);
    let (_, second) = json_run(&repo, "stop");
    assert_eq!(
        second["resources"],
        json!([{"name": "db", "action": "start", "status": "stopped"}])
    );

    let (code, started) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{started}");
    let names: Vec<&str> = started["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .map(|resource| resource["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["db", "web"]);
}

#[test]
fn a_failed_start_exits_1_with_one_json_document_and_is_retried() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB);
    write_resource(&store, "web", WEB);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "db.start", "x"]);
    newgit_ok(&repo, &["action", "web.serve", "x"]);
    newgit_ok(&repo, &["stop", "x"]);

    // While stopped, `web`'s definition changes to one that cannot start.
    write_resource(
        &store,
        "web",
        &WEB.replace(
            "long_running = true",
            "long_running = true\nworkdir = \"nope\"",
        ),
    );

    let (code, started) = json_run(&repo, "start");
    assert_eq!(code, Some(1), "{started}");
    assert_eq!(started["instance"], "x");
    let resources = started["resources"].as_array().expect("resources");
    assert_eq!(resources.len(), 2, "{started}");
    assert_eq!(
        resources[0],
        json!({"name": "db", "action": "start", "status": "started"})
    );
    assert_eq!(resources[1]["name"], "web");
    assert_eq!(resources[1]["action"], "serve");
    assert_eq!(resources[1]["status"], "failed");
    assert!(
        resources[1]["error"]
            .as_str()
            .is_some_and(|error| error.contains("nope")),
        "the error says why: {started}"
    );
    assert!(running(&store, "db") && !running(&store, "web"));

    // The failure stays owed: once the definition is fixed, the next start
    // brings back `web`, and only `web`.
    write_resource(&store, "web", WEB);
    let (code, retried) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{retried}");
    assert_eq!(
        retried["resources"],
        json!([{"name": "web", "action": "serve", "status": "started"}])
    );
    assert!(running(&store, "web"));
}

#[test]
fn an_unknown_instance_is_an_error_like_any_other_command() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    for verb in ["stop", "start"] {
        let run = newgit(&repo, &[verb, "nope", "--json"]);
        assert!(!run.status.success(), "`{verb} nope` must fail");
        assert!(
            run.stderr.contains("nope"),
            "`{verb} nope` names the instance: {}",
            run.stderr
        );
    }
}
