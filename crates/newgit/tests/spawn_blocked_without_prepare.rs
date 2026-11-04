//! Regression test for #209: a dependent with no runnable `prepare` is
//! recorded `ready` when its dependency fails (nothing was withheld from
//! it — see #28), but `status` shows it `blocked(...)` and `action` refuses
//! its command actions. `spawn --json` read the record's `ready` straight
//! through, with an empty `blocked_by`, so a script saw one instance and
//! `status` another. The human-readable `spawn` printed `prepare: BLOCKED`
//! for a resource with no `prepare` and then left it out of the "did not
//! bind" count on the next line.
//!
//! Every surface now derives blocked-ness from the graph the same way.
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
        "newgit-spawn-blocked-without-prepare-{}-{n}",
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
    stdout: String,
    stderr: String,
}

fn run_newgit(repo: &Utf8Path, args: &[&str]) -> SpawnResult {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    SpawnResult {
        status: output.status,
        stdout: String::from_utf8(output.stdout).expect("utf8 stdout"),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The `resources` entry naming `name` in a `--json` document.
fn resource<'a>(json: &'a Value, name: &str, stdout: &str) -> &'a Value {
    json["resources"]
        .as_array()
        .and_then(|resources| resources.iter().find(|resource| resource["name"] == name))
        .unwrap_or_else(|| panic!("no `resources` entry for `{name}`:\n{stdout}"))
}

/// The issue's reproduction, verbatim.
const BASE: &str = r#"ownership = "branch"

[actions.prepare]
command = "exit 1"
"#;

const DEP: &str = r#"ownership = "branch"
depends_on = ["base"]

[actions.start]
command = "sleep 1000"
long_running = true
"#;

#[test]
fn spawn_json_calls_a_dependent_without_prepare_blocked_like_status_does() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "base", BASE);
    write_resource(&store, "dep", DEP);
    let repo = store.paths().project_root.clone();

    let result = run_newgit(&repo, &["spawn", "y", "--json"]);
    assert_eq!(
        result.status.code(),
        Some(1),
        "`base` failed:\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    let json: Value = serde_json::from_str(&result.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}):\n{}\nstderr:\n{}",
            result.stdout, result.stderr
        )
    });

    // The record says `ready` — nothing was withheld from `dep` — but the
    // script must see what `status` and `action` see: blocked, by `base`.
    let dep = resource(&json, "dep", &result.stdout);
    assert_eq!(dep["status"], "blocked", "{}", result.stdout);
    assert_eq!(dep["blocked_by"], json!(["base"]), "{}", result.stdout);
    assert_eq!(dep["prepare_log"], Value::Null, "{}", result.stdout);
    let base = resource(&json, "base", &result.stdout);
    assert_eq!(base["status"], "failed", "{}", result.stdout);
    assert_eq!(base["blocked_by"], json!([]), "{}", result.stdout);

    // And `status` agrees, as it did before.
    let status = run_newgit(&repo, &["status"]);
    assert!(
        status.stdout.contains("dep:blocked(base)"),
        "stdout:\n{}\nstderr:\n{}",
        status.stdout,
        status.stderr
    );
}

#[test]
fn spawn_counts_a_blocked_dependent_without_prepare_as_not_bound() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "base", BASE);
    write_resource(&store, "dep", DEP);
    let repo = store.paths().project_root.clone();

    let result = run_newgit(&repo, &["spawn", "x"]);
    assert_eq!(
        result.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    // `dep` has no `prepare`, so the line must not claim one was blocked.
    assert!(
        !result.stdout.contains("prepare: BLOCKED"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("`dep` BLOCKED by base"),
        "{}",
        result.stdout
    );
    // The summary counts what the lines above it say: both resources.
    assert!(
        result.stdout.contains("2 of 2 resources did not bind"),
        "{}",
        result.stdout
    );
}

#[test]
fn spawn_still_names_prepare_when_a_blocked_dependent_has_one() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "base", BASE);
    write_resource(
        &store,
        "dep",
        r#"ownership = "branch"
depends_on = ["base"]

[actions.prepare]
command = "true"
"#,
    );
    let repo = store.paths().project_root.clone();

    let result = run_newgit(&repo, &["spawn", "x"]);
    assert_eq!(result.status.code(), Some(1), "{}", result.stdout);
    assert!(
        result.stdout.contains("`dep` prepare: BLOCKED by base"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("2 of 2 resources did not bind"),
        "{}",
        result.stdout
    );
}

/// The issue's third surface: `action` refuses a command action on the
/// dependent, whatever the record stores for it.
#[test]
fn action_refuses_a_blocked_dependent_without_prepare() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "base", BASE);
    write_resource(&store, "dep", DEP);
    let repo = store.paths().project_root.clone();

    let spawned = run_newgit(&repo, &["spawn", "x"]);
    assert_eq!(spawned.status.code(), Some(1), "{}", spawned.stdout);

    let result = run_newgit(&repo, &["action", "dep.start", "x"]);
    assert!(
        !result.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    assert!(
        result
            .stderr
            .contains("blocked by failed dependency/dependencies: base"),
        "{}",
        result.stderr
    );
}

/// The #124 shape: a dependent with no actions at all, only `depends_on`.
/// It has nothing to run, and is still blocked on every surface.
#[test]
fn a_bare_dependent_with_no_actions_is_blocked_too() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "base", BASE);
    write_resource(
        &store,
        "dep",
        r#"ownership = "branch"
depends_on = ["base"]
"#,
    );
    let repo = store.paths().project_root.clone();

    let result = run_newgit(&repo, &["spawn", "y", "--json"]);
    assert_eq!(result.status.code(), Some(1), "{}", result.stdout);
    let json: Value = serde_json::from_str(&result.stdout).expect("one JSON document");
    let dep = resource(&json, "dep", &result.stdout);
    assert_eq!(dep["status"], "blocked", "{}", result.stdout);
    assert_eq!(dep["blocked_by"], json!(["base"]), "{}", result.stdout);

    let result = run_newgit(&repo, &["spawn", "x"]);
    assert_eq!(result.status.code(), Some(1), "{}", result.stdout);
    assert!(
        result.stdout.contains("`dep` BLOCKED by base"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("2 of 2 resources did not bind"),
        "{}",
        result.stdout
    );

    let status = run_newgit(&repo, &["status"]);
    assert!(
        status.stdout.contains("dep:blocked(base)"),
        "{}",
        status.stdout
    );
}

/// A resource whose own render failed, behind a failed dependency, is
/// `failed` on every surface — its own failure comes first, because
/// repairing the dependency would not fix it — with the dependency still
/// named in `--json`'s `blocked_by`.
#[test]
fn a_resource_failed_itself_is_failed_everywhere_even_behind_a_failed_dependency() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "base", BASE);
    write_resource(
        &store,
        "dep",
        r#"ownership = "branch"
depends_on = ["base"]

[[render]]
path = "not/committed.toml"
replace = [
  { find = "port = 1", with = "port = 2" },
]
"#,
    );
    let repo = store.paths().project_root.clone();

    let result = run_newgit(&repo, &["spawn", "y", "--json"]);
    assert_eq!(result.status.code(), Some(1), "{}", result.stdout);
    let json: Value = serde_json::from_str(&result.stdout).expect("one JSON document");
    let dep = resource(&json, "dep", &result.stdout);
    assert_eq!(dep["status"], "failed", "{}", result.stdout);
    assert_eq!(dep["blocked_by"], json!(["base"]), "{}", result.stdout);

    let result = run_newgit(&repo, &["spawn", "x"]);
    assert_eq!(result.status.code(), Some(1), "{}", result.stdout);
    assert!(
        result.stdout.contains("`dep` render: FAILED"),
        "{}",
        result.stdout
    );

    let status = run_newgit(&repo, &["status"]);
    assert!(
        status.stdout.contains("dep:failed"),
        "stdout:\n{}\nstderr:\n{}",
        status.stdout,
        status.stderr
    );
}
