//! `newgit start` (#54): bringing long-running resources up in dependency
//! order, gated on a declared `[ready]` probe rather than "the process
//! exists" — see the design note in the PR body for why. New tests, so a
//! new file: see AGENTS.md "Working in parallel".

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, Readiness, StartResult};
use newgit_core::resource::GraphProblem;
use newgit_core::store::MetadataStore;
use newgit_core::{NewgitError, SourceSubstrate};

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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// `newgit start` with no `[ready]` declared confirms liveness, not what the
/// process has gotten around to doing — so a test asserting what a command
/// wrote has to poll for it, the same way a real caller would.
fn wait_for_file(path: &Utf8Path) {
    for _ in 0..50 {
        if path.is_file() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("{path} never appeared");
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

/// `deps` prepares instantly; `db` and `api` are long-running, `api`
/// `start_after`s `db`, and each writes a marker file once its own `start`
/// has actually run, so ordering is checked by file timestamps rather than
/// by trusting the report.
const DB_RESOURCE: &str = r#"ownership = "branch"

[actions.start]
command = "touch db-started.txt && sleep 30"
long_running = true

[actions.stop]
signal = "term"
"#;

const API_DEPENDS_AND_STARTS_AFTER_DB: &str = r#"ownership = "branch"
depends_on = ["db"]
start_after = ["db"]

[actions.start]
command = "touch api-started.txt && sleep 30"
long_running = true

[actions.stop]
signal = "term"
"#;

#[test]
fn start_brings_resources_up_in_dependency_order_and_reports_alive_only_with_no_probe() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB_RESOURCE);
    write_resource(&store, "api", API_DEPENDS_AND_STARTS_AFTER_DB);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    let spawned = manager.spawn("feature-a", None).expect("spawn");

    let outcome = manager.start("feature-a").expect("start");
    assert_eq!(outcome.instance, "feature-a");

    let db = outcome
        .results
        .iter()
        .find(|result| result.name() == "db")
        .expect("db result");
    let api = outcome
        .results
        .iter()
        .find(|result| result.name() == "api")
        .expect("api result");
    assert!(matches!(
        db,
        StartResult::Started {
            readiness: Readiness::AliveOnly,
            ..
        }
    ));
    assert!(
        matches!(
            api,
            StartResult::Started {
                readiness: Readiness::AliveOnly,
                ..
            }
        ),
        "api: {api:?}"
    );

    // `newgit start` only ever confirms these are *alive* — with no
    // `[ready]` it does not wait for `touch` to land, only for `sh -c` to be
    // spawned — so the marker files themselves are polled for briefly
    // rather than asserted the instant `start` returns.
    let db_marker = spawned.branch.workspace_path.join("db-started.txt");
    let api_marker = spawned.branch.workspace_path.join("api-started.txt");
    wait_for_file(&db_marker);
    wait_for_file(&api_marker);
    let db_time = std::fs::metadata(&db_marker)
        .expect("db metadata")
        .modified()
        .expect("db mtime");
    let api_time = std::fs::metadata(&api_marker)
        .expect("api metadata")
        .modified()
        .expect("api mtime");
    assert!(
        db_time <= api_time,
        "db's start_after dependent must not start before db does"
    );

    // Stop both so the sandbox does not leak `sleep 30` past the test.
    manager.run_action("feature-a", "api.stop").expect("stop");
    manager.run_action("feature-a", "db.stop").expect("stop");
}

#[test]
fn start_reports_already_running_on_a_second_call() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    manager.start("feature-a").expect("first start");
    let second = manager.start("feature-a").expect("second start");
    let db = second
        .results
        .iter()
        .find(|result| result.name() == "db")
        .expect("db result");
    assert!(matches!(db, StartResult::AlreadyRunning { .. }));

    manager.run_action("feature-a", "db.stop").expect("stop");
}

/// A resource whose `start` never runs (no command) is reported, not
/// silently skipped — `newgit start` naming every resource it looked at is
/// how a definition typo ("did I forget `long_running`?") surfaces.
#[test]
fn a_resource_with_no_long_running_start_is_reported_not_orchestrated() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "static-config",
        r#"ownership = "branch"

[actions.prepare]
command = "true"
"#,
    );
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    let outcome = manager.start("feature-a").expect("start");
    assert!(matches!(
        outcome.results.first(),
        Some(StartResult::NotOrchestrated { name }) if name == "static-config"
    ));
}

/// `[ready] probe = "command"` gates `newgit start` on more than "the
/// process exists": here `db`'s command only starts answering after a
/// marker file appears, so waiting on it (rather than on liveness) is the
/// only way `api`, which `start_after`s `db`, could ever see it pass.
#[test]
fn a_declared_ready_probe_is_waited_on_before_a_start_after_dependent_runs() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "db",
        r#"ownership = "branch"

[actions.start]
command = "(sleep 1 && touch became-ready.txt &) ; sleep 30"
long_running = true

[actions.stop]
signal = "term"

[ready]
probe = "command"
command = "test -f became-ready.txt"
timeout_secs = 10
interval_ms = 100
"#,
    );
    write_resource(&store, "api", API_DEPENDS_AND_STARTS_AFTER_DB);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    let outcome = manager.start("feature-a").expect("start");
    let db = outcome
        .results
        .iter()
        .find(|result| result.name() == "db")
        .expect("db result");
    let api = outcome
        .results
        .iter()
        .find(|result| result.name() == "api")
        .expect("api result");
    assert!(
        matches!(
            db,
            StartResult::Started {
                readiness: Readiness::Ready,
                ..
            }
        ),
        "db: {db:?}"
    );
    assert!(
        matches!(
            api,
            StartResult::Started {
                readiness: Readiness::AliveOnly,
                ..
            }
        ),
        "api declares no [ready] of its own: {api:?}"
    );

    manager.run_action("feature-a", "api.stop").expect("stop");
    manager.run_action("feature-a", "db.stop").expect("stop");
}

/// A `[ready]` probe that never passes times out rather than hanging
/// `newgit start` forever, and names the resource and the probe.
#[test]
fn a_ready_probe_that_never_passes_times_out_and_is_reported_failed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "db",
        r#"ownership = "branch"

[actions.start]
command = "sleep 30"
long_running = true

[actions.stop]
signal = "term"

[ready]
probe = "command"
command = "false"
timeout_secs = 1
interval_ms = 100
"#,
    );
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    let outcome = manager.start("feature-a").expect("start");
    let db = outcome
        .results
        .iter()
        .find(|result| result.name() == "db")
        .expect("db result");
    match db {
        StartResult::Failed { reason, .. } => {
            assert!(reason.contains("did not become ready"), "{reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    manager.run_action("feature-a", "db.stop").expect("stop");
}

/// `start_after` naming something outside `depends_on` is refused at load —
/// a start-order dependency with no lifecycle ordering behind it is never
/// what was meant, and it would otherwise have to invent an order of its
/// own instead of reusing `depends_on`'s.
#[test]
fn start_after_must_already_be_in_depends_on() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "api",
        r#"ownership = "branch"
start_after = ["db"]

[actions.start]
command = "sleep 30"
long_running = true
"#,
    );
    let repo = store.paths().project_root.clone();
    let error =
        BranchManager::open(MetadataStore::at(repo)).expect_err("start_after not in depends_on");
    assert!(error.to_string().contains("start_after"), "{error}");
}

/// `start_after` naming a resource with no `long_running` `start` is a
/// graph problem: there would be nothing for `newgit start` to wait on.
/// This is cross-resource, so it is caught by the graph, not by loading one
/// definition in isolation.
#[test]
fn start_after_naming_a_resource_with_no_long_running_start_is_a_graph_problem() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "config",
        r#"ownership = "branch"

[actions.prepare]
command = "true"
"#,
    );
    write_resource(
        &store,
        "api",
        r#"ownership = "branch"
depends_on = ["config"]
start_after = ["config"]

[actions.start]
command = "sleep 30"
long_running = true
"#,
    );
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager opens");

    assert!(
        manager.graph_problems().iter().any(|problem| matches!(
            problem,
            GraphProblem::StartAfterNotOrchestrated { resource, dependency }
                if resource == "api" && dependency == "config"
        )),
        "{:?}",
        manager.graph_problems()
    );
    assert!(matches!(
        manager.start("feature-a"),
        Err(NewgitError::StartAfterNotOrchestrated { .. })
    ));
}

/// Only `newgit start` enforces `start_after` — `newgit action` runs the
/// single action asked for, exactly as it always has, so a hand-invoked
/// `api.start` still runs even with `db` never started. `start_after` is
/// `newgit start`'s contract, not a hidden gate on the action itself; that
/// boundary is worth pinning down in a test, since it is easy to assume
/// otherwise.
#[test]
fn newgit_action_does_not_enforce_start_after_only_newgit_start_does() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB_RESOURCE);
    write_resource(&store, "api", API_DEPENDS_AND_STARTS_AFTER_DB);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    manager
        .run_action("feature-a", "api.start")
        .expect("depends_on's own gate (blocked_by) still applies, and db prepared fine");

    manager.run_action("feature-a", "api.stop").expect("stop");
}

/// The `tcp`/`http` probes are real network checks, not just a code path —
/// this exercises one end to end against a real listener, skipped where
/// `python3` is not on `PATH` rather than failing the whole suite over a
/// tool this test alone needs.
#[test]
fn an_http_ready_probe_waits_for_the_port_to_actually_answer() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }

    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "web",
        r#"ownership = "branch"

[ports.web]
start = 42100

[actions.start]
command = "python3 -m http.server {{ports.web}} --bind 127.0.0.1"
long_running = true

[actions.stop]
signal = "term"

[ready]
probe = "http"
port = "web"
path = "/"
timeout_secs = 10
interval_ms = 100
"#,
    );
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    let outcome = manager.start("feature-a").expect("start");
    let web = outcome
        .results
        .iter()
        .find(|result| result.name() == "web")
        .expect("web result");
    assert!(
        matches!(
            web,
            StartResult::Started {
                readiness: Readiness::Ready,
                ..
            }
        ),
        "web: {web:?}"
    );

    manager.run_action("feature-a", "web.stop").expect("stop");
}
