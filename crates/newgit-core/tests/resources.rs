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

const BAD_PREP_RESOURCE: &str = r#"kind = "command"
ownership = "workspace"

[actions.prepare]
command = "echo bad-prep-ran > bad-prep.txt; exit 7"
"#;

const AFTER_BAD_RESOURCE: &str = r#"kind = "command"
ownership = "workspace"
depends_on = ["bad-prep"]

[actions.prepare]
command = "echo should-not-run > after-bad.txt"
"#;

const BLOCKED_PROCESS_RESOURCE: &str = r#"kind = "process"
ownership = "branch"
depends_on = ["bad-prep"]

[actions.start]
command = "sleep 30"
long_running = true

[actions.stop]
signal = "term"
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
fn failed_prepare_blocks_dependents_but_keeps_instance_spawned() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "bad-prep", BAD_PREP_RESOURCE);
    write_resource(&store, "after-bad", AFTER_BAD_RESOURCE);
    write_resource(&store, "blocked-app", BLOCKED_PROCESS_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let spawned = manager.spawn("feature-a", None).expect("spawn");

    assert!(spawned.branch.workspace_path.is_dir());
    assert_eq!(
        spawned.branch.resources["bad-prep"].status,
        ResourceStatus::Failed
    );
    assert_eq!(
        spawned.branch.resources["after-bad"].status,
        ResourceStatus::Blocked
    );
    assert_eq!(
        spawned.branch.resources["blocked-app"].status,
        ResourceStatus::Blocked
    );
    assert!(
        spawned.branch.workspace_path.join("bad-prep.txt").is_file(),
        "the failing dependency should have run"
    );
    assert!(
        !spawned.branch.workspace_path.join("after-bad.txt").exists(),
        "blocked dependents should not run prepare"
    );

    let reports = manager.statuses().expect("statuses");
    let resources = &reports[0].resources;
    assert_eq!(
        resources
            .iter()
            .find(|r| r.name == "bad-prep")
            .expect("bad-prep")
            .state,
        "failed"
    );
    assert_eq!(
        resources
            .iter()
            .find(|r| r.name == "after-bad")
            .expect("after-bad")
            .state,
        "blocked"
    );
    assert_eq!(
        resources
            .iter()
            .find(|r| r.name == "blocked-app")
            .expect("blocked-app")
            .state,
        "blocked"
    );
    assert!(matches!(
        manager.run_action("feature-a", "after-bad.prepare"),
        Err(NewgitError::Unsupported(_))
    ));
    assert!(matches!(
        manager.run_action("feature-a", "blocked-app.start"),
        Err(NewgitError::Unsupported(_))
    ));
}

#[test]
fn run_command_sees_layered_env() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", APP_RESOURCE);
    write_resource(&store, "prep", PREP_RESOURCE);

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
                "echo \"$PORT|$APP_URL|$NEWGIT_BRANCH\" > env-probe.txt".to_owned(),
            ],
        )
        .expect("run");
    assert_eq!(code, 0);

    let probe =
        std::fs::read_to_string(spawned.branch.workspace_path.join("env-probe.txt")).expect("read");
    assert_eq!(
        probe.trim(),
        format!("{port}|http://127.0.0.1:{port}/feature-a|feature-a")
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
fn pnpm_template_creates_companion_store_and_loads() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("manager");

    let outcome = manager.add_resource("deps", "pnpm").expect("add pnpm");
    assert!(outcome.path.is_file());
    assert_eq!(outcome.companions_created.len(), 1);
    assert!(
        outcome.companions_created[0]
            .as_str()
            .ends_with("pnpm-store.toml")
    );

    // Definitions load and the dependency resolves (no MissingDependency).
    let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("reopen");
    let names: Vec<&str> = manager
        .resource_definitions()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert!(names.contains(&"deps"));
    assert!(names.contains(&"pnpm-store"));

    // Re-adding under another name must not overwrite the existing companion.
    let again = manager.add_resource("deps2", "pnpm").expect("add again");
    assert!(again.companions_created.is_empty());
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

/// Every shipped template has to survive the trip through the parser and the
/// dependency graph, or `newgit resource add` hands the user a project that
/// will not open. This is the "model a normal web app without writing TOML
/// from scratch" claim, checked.
#[test]
fn every_template_loads_after_being_added() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    for template in newgit_core::templates::RESOURCE_TEMPLATES {
        let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("reopen");
        manager
            .add_resource(template.name, template.name)
            .unwrap_or_else(|error| panic!("add `{}`: {error}", template.name));
    }

    // One project holding all of them still opens: definitions parse, every
    // `depends_on` resolves, and no two lanes claim the same path.
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("open with all templates");
    let names: Vec<&str> = manager
        .resource_definitions()
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    for template in newgit_core::templates::RESOURCE_TEMPLATES {
        assert!(
            names.contains(&template.name),
            "{} is missing",
            template.name
        );
    }
}

#[test]
fn the_command_snapshot_template_brings_its_deposit_lane() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("manager");

    let outcome = manager
        .add_resource("postgres-db", "command-snapshot")
        .expect("add");
    assert_eq!(outcome.trackers_created.len(), 1);
    assert!(
        outcome.trackers_created[0]
            .as_str()
            .ends_with("db-snapshots.toml"),
        "a template that deposits must create the lane it deposits into"
    );

    // The lane exists as a deposit-only tracker: no owned workspace paths.
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let lane = manager
        .tracker_definitions()
        .iter()
        .find(|definition| definition.name == "db-snapshots")
        .expect("db-snapshots defined");
    assert!(lane.paths.is_empty());
    assert_eq!(lane.audience, "project-devs");

    // Adding a second database resource reuses the existing lane.
    let again = manager
        .add_resource("other-db", "command-snapshot")
        .expect("add again");
    assert!(again.trackers_created.is_empty());
}

#[test]
fn captures_publish_a_handle_into_the_binding_and_the_command_env() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    // KEY=VALUE lines, the other accepted capture shape.
    write_resource(
        &store,
        "preview",
        r#"kind = "external"
ownership = "external"

[actions.prepare]
command = "echo PREVIEW_URL=https://pv9.example"
captures = ["PREVIEW_URL"]
"#,
    );
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let spawned = manager.spawn("feature-a", None).expect("spawn");
    assert_eq!(
        spawned.branch.resources["preview"].resolved_exports["PREVIEW_URL"],
        "https://pv9.example"
    );

    // Persisted on the binding record, and layered into the command env.
    let reloaded = manager.store().find_branch("feature-a").expect("reload");
    assert_eq!(
        reloaded.resources["preview"].resolved_exports["PREVIEW_URL"],
        "https://pv9.example"
    );
    let env = manager.assemble_env(&reloaded).expect("env");
    assert!(env.contains(&("PREVIEW_URL".to_owned(), "https://pv9.example".to_owned())));
}
