//! Regression test: `newgit spawn --json` reported success for a resource
//! that failed to bind. #86 made the human-readable `spawn` exit 1 in that
//! case (see `spawn_exit.rs`), but the `--json` branch printed its object and
//! returned before that check, and the object carried no resource status at
//! all — so a failed `prepare` exited 0 with nothing in the output saying so,
//! to the caller most likely to ask for JSON: a script.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::ResourceStatus;
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

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops. Hand-rolled rather than pulling in the `tempfile`
/// dev-dependency other crates use — matches `spawn_exit.rs`.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "newgit-spawn-json-failure-{}-{n}",
        std::process::id()
    ));
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

struct SpawnResult {
    status: std::process::ExitStatus,
    json: Value,
    stdout: String,
    stderr: String,
}

/// `newgit spawn <name> --json`, its stdout parsed as one JSON document —
/// which is itself an assertion: nothing may be printed beside it.
fn run_spawn_json(repo: &Utf8Path, name: &str) -> SpawnResult {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(["spawn", name, "--json"])
        .output()
        .expect("newgit runs");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let json = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("stdout is not one JSON document ({error}):\n{stdout}\nstderr:\n{stderr}")
    });
    SpawnResult {
        status: output.status,
        json,
        stdout,
        stderr,
    }
}

impl SpawnResult {
    /// The `resources` entry naming `name`.
    fn resource(&self, name: &str) -> &Value {
        self.json["resources"]
            .as_array()
            .and_then(|resources| resources.iter().find(|resource| resource["name"] == name))
            .unwrap_or_else(|| panic!("no `resources` entry for `{name}`:\n{}", self.stdout))
    }
}

const FAILING_RESOURCE: &str = r#"ownership = "branch"

[actions.prepare]
command = "exit 1"
"#;

#[test]
fn spawn_json_does_not_report_success_when_a_resource_fails_to_bind() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "failing-deps", FAILING_RESOURCE);
    let repo = store.paths().project_root.clone();

    let result = run_spawn_json(&repo, "x");

    // The record says what happened, so the failure is real and known. It is
    // still there: exit 1 reports the failure, it does not roll it back.
    let branches = store.load_branches().expect("load branches");
    let record = branches
        .iter()
        .find(|branch| branch.name == "x")
        .expect("`x` was spawned, and kept");
    assert_eq!(
        record.resources["failing-deps"].status,
        ResourceStatus::Failed,
        "precondition: the prepare failed"
    );

    // A script needs both answers: the exit code to know something did not
    // come up, and the JSON to know which and where to look.
    assert_eq!(
        result.status.code(),
        Some(1),
        "`newgit spawn x --json` must exit 1 when `failing-deps`'s prepare failed, as \
         `newgit spawn x` does:\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    let failing = result.resource("failing-deps");
    assert_eq!(failing["status"], "failed", "{}", result.stdout);
    let log = failing["prepare_log"]
        .as_str()
        .unwrap_or_else(|| panic!("the failed prepare's log is named:\n{}", result.stdout));
    assert!(
        Utf8Path::new(log).is_file(),
        "`prepare_log` should be the log the prepare wrote: {log}"
    );
}

#[test]
fn spawn_json_says_why_each_resource_did_not_come_up() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "base", FAILING_RESOURCE);
    write_resource(
        &store,
        "dependent",
        r#"ownership = "branch"
depends_on = ["base"]

[actions.prepare]
command = "true"
"#,
    );
    write_resource(
        &store,
        "unexported",
        r#"ownership = "branch"

[exports]
URL = "http://127.0.0.1:{{ports.nope}}"
"#,
    );
    write_resource(
        &store,
        "unrendered",
        r#"ownership = "branch"

[[render]]
path = "not/committed.toml"
replace = [
  { find = "port = 1", with = "port = 2" },
]
"#,
    );
    let repo = store.paths().project_root.clone();

    let result = run_spawn_json(&repo, "x");
    assert_eq!(
        result.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );

    // Never stored as such, but what the human-readable line calls it.
    let dependent = result.resource("dependent");
    assert_eq!(dependent["status"], "blocked", "{}", result.stdout);
    assert_eq!(
        dependent["blocked_by"],
        json!(["base"]),
        "{}",
        result.stdout
    );
    assert_eq!(
        dependent["prepare_log"],
        Value::Null,
        "a blocked prepare never ran:\n{}",
        result.stdout
    );

    let unexported = result.resource("unexported");
    assert_eq!(unexported["status"], "failed", "{}", result.stdout);
    assert!(
        unexported["export_error"]
            .as_str()
            .is_some_and(|error| error.contains("URL")),
        "the export that did not resolve is named:\n{}",
        result.stdout
    );

    let unrendered = result.resource("unrendered");
    assert_eq!(unrendered["status"], "failed", "{}", result.stdout);
    assert!(
        unrendered["render_error"]
            .as_str()
            .is_some_and(|error| error.contains("not/committed.toml")),
        "the file that did not render is named:\n{}",
        result.stdout
    );
}

#[test]
fn spawn_json_exits_zero_and_reports_ready_when_every_resource_binds() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "deps",
        r#"ownership = "branch"

[ports]
app = { start = 4200 }

[actions.prepare]
command = "true"
"#,
    );
    let repo = store.paths().project_root.clone();

    let result = run_spawn_json(&repo, "x");
    assert!(
        result.status.success(),
        "expected exit 0, got {:?}\nstdout:\n{}\nstderr:\n{}",
        result.status.code(),
        result.stdout,
        result.stderr
    );
    let deps = result.resource("deps");
    assert_eq!(deps["status"], "ready", "{}", result.stdout);
    assert!(deps["ports"]["app"].is_u64(), "{}", result.stdout);
    assert!(deps["prepare_log"].is_string(), "{}", result.stdout);
    assert_eq!(deps["blocked_by"], json!([]), "{}", result.stdout);
    assert_eq!(deps["render_error"], Value::Null, "{}", result.stdout);
    assert_eq!(deps["export_error"], Value::Null, "{}", result.stdout);
}
