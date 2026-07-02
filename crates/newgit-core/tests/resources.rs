use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::branch::ResourceStatus;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{ActionOutcome, BranchManager};
use newgit_core::store::MetadataStore;
use newgit_core::supervisor::StopOutcome;
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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

const APP_RESOURCE: &str = r#"kind = "process"
ownership = "branch"
depends_on = ["prep"]

[ports]
app = { start = 3900, env = "PORT" }

[actions.start]
command = "sleep 30"
long_running = true

[actions.stop]
signal = "term"

[exports]
APP_URL = "http://127.0.0.1:{{ports.app}}/{{branch.slug}}"
"#;

const PREP_RESOURCE: &str = r#"kind = "command"
ownership = "workspace"

[actions.prepare]
command = "echo prepared-{{branch.slug}} > prepared.txt"
"#;

#[test]
fn spawn_allocates_stable_distinct_ports_and_runs_prepare() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", APP_RESOURCE);
    write_resource(&store, "prep", PREP_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let a = manager.spawn("feature-a", None).expect("spawn a");
    let b = manager.spawn("feature-b", None).expect("spawn b");

    let port_a = a.branch.resources["app"].resolved_ports["app"];
    let port_b = b.branch.resources["app"].resolved_ports["app"];
    assert!(port_a >= 3900);
    assert_ne!(port_a, port_b, "instances must not share a port");

    // Exports rendered with the allocated port and branch vars.
    assert_eq!(
        a.branch.resources["app"].resolved_exports["APP_URL"],
        format!("http://127.0.0.1:{port_a}/feature-a")
    );

    // Prepare ran in dependency order and left its artifact; status ready.
    assert_eq!(
        std::fs::read_to_string(a.branch.workspace_path.join("prepared.txt")).expect("read"),
        "prepared-feature-a\n"
    );
    assert_eq!(a.branch.resources["prep"].status, ResourceStatus::Ready);

    // Persisted: reloading the record shows the same port (determinism).
    let reloaded = manager.store().find_branch("feature-a").expect("reload");
    assert_eq!(reloaded.resources["app"].resolved_ports["app"], port_a);
}

#[test]
fn run_command_sees_layered_env() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", APP_RESOURCE);
    write_resource(&store, "prep", PREP_RESOURCE);
    // An env-file tracker to exercise layer 1.
    std::fs::write(
        store.paths().trackers.join("runtime-env.toml"),
        r#"kind = "env-file"
audience = "user"
storage = "local"
propagation = "pin"
paths = [".env"]

[materialize]
copy_from = ".newgit/templates/base.env"
to = ".env"

[exports]
env_file = ".env"
"#,
    )
    .expect("write tracker");
    std::fs::write(
        store.paths().templates.join("base.env"),
        "FROM_ENV_FILE=yes\nDATABASE_URL='postgres://localhost/base'\n",
    )
    .expect("write env template");
    std::fs::write(store.paths().project_root.join(".gitignore"), "/.env\n").expect("gitignore");

    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let port = spawned.branch.resources["app"].resolved_ports["app"];

    let (code, _log) = manager
        .run_command(
            "feature-a",
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "echo \"$FROM_ENV_FILE|$DATABASE_URL|$PORT|$APP_URL|$NEWGIT_BRANCH\" > env-probe.txt"
                    .to_owned(),
            ],
        )
        .expect("run");
    assert_eq!(code, 0);

    let probe =
        std::fs::read_to_string(spawned.branch.workspace_path.join("env-probe.txt")).expect("read");
    assert_eq!(
        probe.trim(),
        format!("yes|postgres://localhost/base|{port}|http://127.0.0.1:{port}/feature-a|feature-a")
    );
}

#[test]
fn supervisor_starts_and_stops_long_running_actions() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", APP_RESOURCE);
    write_resource(&store, "prep", PREP_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    let ActionOutcome::Started { pid, .. } =
        manager.run_action("feature-a", "app.start").expect("start")
    else {
        panic!("expected Started");
    };
    assert!(pid > 0);

    // Double-start is refused.
    assert!(matches!(
        manager.run_action("feature-a", "app.start"),
        Err(NewgitError::AlreadyRunning { .. })
    ));

    // Status reflects the live process.
    let reports = manager.statuses().expect("statuses");
    let app_state = reports[0]
        .resources
        .iter()
        .find(|r| r.name == "app")
        .expect("app report");
    assert_eq!(app_state.state, "running");

    let ActionOutcome::Stopped(outcome) =
        manager.run_action("feature-a", "app.stop").expect("stop")
    else {
        panic!("expected Stopped");
    };
    assert_eq!(outcome, StopOutcome::Stopped(pid));

    let reports = manager.statuses().expect("statuses");
    let app_state = reports[0]
        .resources
        .iter()
        .find(|r| r.name == "app")
        .expect("app report");
    assert_eq!(app_state.state, "stopped");
}

#[test]
fn remove_stops_running_processes() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", APP_RESOURCE);
    write_resource(&store, "prep", PREP_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    let ActionOutcome::Started { pid, .. } =
        manager.run_action("feature-a", "app.start").expect("start")
    else {
        panic!("expected Started");
    };

    manager.remove("feature-a", &temp).expect("remove");

    // The process group is gone.
    let alive = Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .status()
        .expect("kill -0")
        .success();
    assert!(!alive, "process group survived remove");
}

#[test]
fn unknown_action_and_bad_spec_error_cleanly() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", APP_RESOURCE);
    write_resource(&store, "prep", PREP_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    assert!(matches!(
        manager.run_action("feature-a", "app.dance"),
        Err(NewgitError::UnknownAction { .. })
    ));
    assert!(matches!(
        manager.run_action("feature-a", "nope.start"),
        Err(NewgitError::UnknownResource(_))
    ));
    assert!(matches!(
        manager.run_action("feature-a", "no-dot"),
        Err(NewgitError::Unsupported(_))
    ));
}
