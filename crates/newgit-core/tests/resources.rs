use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::branch::ResourceStatus;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{ActionOutcome, BranchManager};
use newgit_core::store::MetadataStore;
use newgit_core::supervisor::StopOutcome;
use newgit_core::tracker::Storage;
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

/// An executable script under the store's `.newgit/scripts/`.
fn write_script(path: &Utf8Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

const APP_RESOURCE: &str = r#"ownership = "branch"
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

const PREP_RESOURCE: &str = r#"ownership = "workspace"

[actions.prepare]
command = "echo prepared-{{branch.slug}} > prepared.txt"
"#;

const BAD_PREP_RESOURCE: &str = r#"ownership = "workspace"

[actions.prepare]
command = "echo bad-prep-ran > bad-prep.txt; exit 7"
"#;

const AFTER_BAD_RESOURCE: &str = r#"ownership = "workspace"
depends_on = ["bad-prep"]

[actions.prepare]
command = "echo should-not-run > after-bad.txt"
"#;

const BLOCKED_PROCESS_RESOURCE: &str = r#"ownership = "branch"
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
    // Blocked-ness is not stored on the resource itself — it's derived from
    // the dependency's status. `after-bad` has a real `prepare` that was
    // withheld, so it stays `Pending`. `blocked-app` has no `prepare` at
    // all — nothing was ever going to run for it regardless of the
    // blocker — so there is nothing to withhold and it is `Ready`
    // immediately, the same as `admin`/`mobile` in the field report.
    assert_eq!(
        spawned.branch.resources["after-bad"].status,
        ResourceStatus::Pending
    );
    assert_eq!(
        spawned.branch.resources["blocked-app"].status,
        ResourceStatus::Ready
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
        "blocked(bad-prep)"
    );
    // `blocked-app`'s own bind status is `Ready` (asserted above) — it has
    // no `prepare` to withhold — but `status` still shows it blocked here,
    // because its dependency `bad-prep` is not ready and that is still a
    // true, useful thing to say about the graph. The stored status and the
    // displayed status answer different questions.
    assert_eq!(
        resources
            .iter()
            .find(|r| r.name == "blocked-app")
            .expect("blocked-app")
            .state,
        "blocked(bad-prep)"
    );
    assert!(matches!(
        manager.run_action("feature-a", "after-bad.prepare"),
        Err(NewgitError::Unsupported(_))
    ));
    // And for the same reason, `start` is a command action gated on that
    // same failed dependency, and correctly still refuses: `blocked-app`
    // being `Ready` is not the same claim as "safe to run a command that
    // assumes `bad-prep` succeeded."
    assert!(matches!(
        manager.run_action("feature-a", "blocked-app.start"),
        Err(NewgitError::Unsupported(_))
    ));
}

/// Issue #28: a resource can declare a `long_running` action that newgit has
/// never started (a stray `functions` action alongside the one actually
/// used). `status` must not read "some action here is long_running" as "this
/// resource is down" — that conflates a declaration with a runtime fact.
#[test]
fn status_does_not_claim_stopped_for_a_long_running_action_never_started() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "supabase",
        r#"ownership = "branch"

[actions.prepare]
command = "true"

[actions.functions]
long_running = true
command = "sleep 30"

[actions.stop]
signal = "term"
"#,
    );
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    // Prepare succeeded and `functions` was never started: this is "ready",
    // not "stopped" — nothing newgit started has exited.
    let reports = manager.statuses().expect("statuses");
    let state = reports[0]
        .resources
        .iter()
        .find(|r| r.name == "supabase")
        .expect("supabase report")
        .state
        .clone();
    assert_eq!(state, "ready");

    // Once something is actually started and stopped, `stopped` becomes
    // honest again.
    manager
        .run_action("feature-a", "supabase.functions")
        .expect("start functions");
    manager
        .run_action("feature-a", "supabase.stop")
        .expect("stop functions");
    let reports = manager.statuses().expect("statuses");
    let state = reports[0]
        .resources
        .iter()
        .find(|r| r.name == "supabase")
        .expect("supabase report")
        .state
        .clone();
    assert_eq!(state, "stopped");
}

/// Issue #28: `newgit cleanup` must not erase the fact `status` now depends
/// on. `retire_dead_pids` used to delete a pid file once its process was
/// confirmed gone — exactly the file whose mere presence means "newgit
/// started this once" — so gc would silently turn `stopped` back into
/// whatever the resource's bind status says (`ready`, here). The fix
/// rewrites the file to `stopped` in place instead of deleting it.
#[test]
fn cleanup_does_not_turn_stopped_back_into_ready() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", APP_RESOURCE);
    write_resource(&store, "prep", PREP_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    manager.run_action("feature-a", "app.start").expect("start");
    manager.run_action("feature-a", "app.stop").expect("stop");

    let state = |manager: &BranchManager| {
        manager.statuses().expect("statuses")[0]
            .resources
            .iter()
            .find(|r| r.name == "app")
            .expect("app report")
            .state
            .clone()
    };
    assert_eq!(state(&manager), "stopped");

    manager
        .cleanup(false, ArchivedCheckpoints::Keep)
        .expect("cleanup");

    assert_eq!(
        state(&manager),
        "stopped",
        "gc must not resurrect a stopped resource as ready by deleting its pid file"
    );
}

/// Issue #28: `blocked` must not survive the blocker clearing. `admin` here
/// has no `prepare` of its own — like the real `admin`/`mobile` resources in
/// the report — so nothing would ever recompute a stored `blocked` status.
/// Recomputing it at read time instead means it falls out of date the moment
/// the dependency does, not the moment something happens to touch `admin`.
#[test]
fn blocked_status_clears_once_the_blocking_dependency_recovers() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "supabase",
        r#"ownership = "branch"

[actions.prepare]
command = "test -f started || (touch started && exit 1)"
"#,
    );
    write_resource(
        &store,
        "admin",
        r#"ownership = "branch"
depends_on = ["supabase"]
"#,
    );
    // A dependent of `admin` itself, to prove the fix does not just move the
    // dead end one hop over: if `admin` stayed stuck, `web` would too.
    write_resource(
        &store,
        "web",
        r#"ownership = "branch"
depends_on = ["admin"]
"#,
    );
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    let spawned = manager.spawn("feature-a", None).expect("spawn");

    // Neither `admin` nor `web` has a `prepare` to withhold, so both are
    // bound `Ready` immediately — there was never a command that could have
    // been blocked. `status` still reports `admin` as blocked while
    // `supabase` is down, because that is a true and useful thing to say
    // about the graph, but the stored fact about `admin` itself is not a
    // dead end the way `Blocked` used to be.
    assert_eq!(
        spawned.branch.resources["admin"].status,
        ResourceStatus::Ready
    );
    assert_eq!(
        spawned.branch.resources["web"].status,
        ResourceStatus::Ready
    );

    let state_of = |manager: &BranchManager, name: &str| {
        manager.statuses().expect("statuses")[0]
            .resources
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} report"))
            .state
            .clone()
    };
    assert_eq!(state_of(&manager, "admin"), "blocked(supabase)");
    // `web` depends on `admin`, not on `supabase` directly. `admin`'s own
    // bind status is `Ready` throughout — the display-only `blocked(...)`
    // above is derived for `admin`'s own row, not stored — so `web` was
    // never blocked by anything and reports `ready` from the start. This is
    // the concrete "usable as a dependency" claim: a no-`prepare` resource
    // does not propagate a graph-wide stall just because something behind
    // it happens to be down.
    assert_eq!(state_of(&manager, "web"), "ready");

    // The user's exact recovery step: re-running the failed prepare.
    manager
        .run_action("feature-a", "supabase.prepare")
        .expect("prepare succeeds the second time");
    assert_eq!(
        manager
            .store()
            .find_branch("feature-a")
            .expect("reload")
            .resources["supabase"]
            .status,
        ResourceStatus::Ready
    );

    // Once `supabase` is ready, nothing is blocking `admin` either: it now
    // reports its own bind status, `ready`, not a stale `blocked` and not a
    // `pending` that nothing would ever clear.
    assert_eq!(state_of(&manager, "admin"), "ready");
    assert_eq!(state_of(&manager, "web"), "ready");
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

    manager
        .remove("feature-a", &temp, ArchivedCheckpoints::Keep)
        .expect("remove");

    // The process group is gone.
    let alive = Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .status()
        .expect("kill -0")
        .success();
    assert!(!alive, "process group survived remove");
}

const WORKDIR_LONG_RUNNING_RESOURCE: &str = r#"ownership = "branch"
workdir = "packages/app"

[actions.start]
command = "pwd; sleep 30"
long_running = true

[actions.stop]
signal = "term"
"#;

#[test]
fn long_running_actions_honor_workdir() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::create_dir_all(repo.join("packages/app")).expect("mkdir");
    std::fs::write(repo.join("packages/app/.keep"), "").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "add packages/app"]);

    write_resource(&store, "app", WORKDIR_LONG_RUNNING_RESOURCE);
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let ws = spawned.branch.workspace_path.clone();

    let ActionOutcome::Started { log, .. } =
        manager.run_action("feature-a", "app.start").expect("start")
    else {
        panic!("expected Started");
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    let logged = std::fs::read_to_string(&log).expect("read log");
    assert!(
        logged.contains(ws.join("packages/app").as_str()),
        "the supervised process should start with cwd = workspace/packages/app: {logged}"
    );

    manager.run_action("feature-a", "app.stop").expect("stop");
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
        r#"ownership = "external"

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

/// A resource that names a tracker before the tracker exists used to break
/// `BranchManager::open`, so every command failed — including the ones that
/// create the missing name. Definition-building commands must stay reachable.
#[test]
fn an_unresolved_dependency_does_not_block_the_commands_that_fix_it() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    write_resource(
        &store,
        "db",
        r#"ownership = "branch"
depends_on = ["runtime-env"]

[actions.prepare]
command = "true"
"#,
    );

    let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("open still works");
    assert_eq!(
        manager.graph_problems(),
        &[newgit_core::resource::GraphProblem::MissingDependency {
            resource: "db".to_owned(),
            dependency: "runtime-env".to_owned(),
        }]
    );

    // Graph-acting commands refuse, and say which name is missing.
    assert!(matches!(
        manager.spawn("blocked", None),
        Err(NewgitError::MissingDependency { .. })
    ));

    // Definition-building commands run, and creating the tracker resolves it.
    manager
        .create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create tracker against an incomplete graph");
    manager
        .track_paths("runtime-env", &[Utf8PathBuf::from("packages/db/.env")])
        .expect("track paths against an incomplete graph");

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    assert!(manager.graph_problems().is_empty());
    manager.spawn("unblocked", None).expect("spawn now works");
}

/// A cycle is reported the same way: `open` succeeds, graph-acting commands
/// refuse. Otherwise a typo in `depends_on` bricks the project.
#[test]
fn a_dependency_cycle_is_reported_rather_than_raised_at_open() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    for (name, dependency) in [("a", "b"), ("b", "a")] {
        write_resource(
            &store,
            name,
            &format!(
                r#"ownership = "branch"
depends_on = ["{dependency}"]

[actions.prepare]
command = "true"
"#
            ),
        );
    }

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("open still works");
    assert!(matches!(
        manager.graph_problems(),
        [newgit_core::resource::GraphProblem::Cycle(_)]
    ));
    assert!(matches!(
        manager.spawn("blocked", None),
        Err(NewgitError::DependencyCycle(_))
    ));
    // Listing definitions is how you find the cycle, so it must not refuse.
    assert_eq!(manager.resource_definitions().len(), 2);
}

/// A resource definition is read from the store, but anything it shelled out
/// to was read from the workspace — so iterating on a `prepare` script meant
/// committing every attempt or copying it into the workspace by hand.
/// `{{scripts}}` puts both halves of a definition under one rule.
#[test]
fn a_scripts_command_picks_up_edits_without_a_commit() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let script = store.paths().scripts.join("prepare.sh");

    write_resource(
        &store,
        "db",
        r#"ownership = "workspace"

[actions.prepare]
command = "{{scripts}}/prepare.sh {{branch.slug}}"
"#,
    );
    write_script(&script, "echo first-$1 > prepared.txt\n");

    // Never committed: the script is not in the source history the workspace
    // clone is made from.
    let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("manager");
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    assert_eq!(
        spawned.branch.resources["db"].status,
        ResourceStatus::Ready,
        "prepare should find the script in the store"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("prepared.txt")).expect("read"),
        "first-feature-a\n"
    );
    assert!(
        !workspace.join(".newgit/scripts/prepare.sh").exists(),
        "the script runs from the store, it is not copied into the workspace"
    );

    // The loop the issue described: edit in place, re-run, no commit, no copy.
    write_script(&script, "echo second-$1 > prepared.txt\n");
    let outcome = manager
        .run_action("feature-a", "db.prepare")
        .expect("re-run prepare");
    assert!(matches!(outcome, ActionOutcome::Ran { code: 0, .. }));
    assert_eq!(
        std::fs::read_to_string(workspace.join("prepared.txt")).expect("read"),
        "second-feature-a\n",
        "the edited script should run, not the one from spawn time"
    );
}

/// A declared capture that never appears in stdout used to pass in total
/// silence: exit 0, `prepare: ok`, resource `ready`, and an empty handle
/// nobody noticed until an API call 401'd. The usual cause is the command's
/// own noise on stdout, which `captures` reserves for newgit.
#[test]
fn a_declared_capture_that_never_appears_is_reported() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    // Emits one of the two declared names, with progress noise around it —
    // exactly the shape that bit in the field.
    write_resource(
        &store,
        "db",
        r#"ownership = "external"

[actions.prepare]
command = "echo 'Starting containers...'; echo ANON_KEY=abc; echo 'done.'"
captures = ["ANON_KEY", "SERVICE_ROLE_KEY"]
"#,
    );
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let resource = spawned
        .resources
        .iter()
        .find(|r| r.name == "db")
        .expect("db bound");

    // What did arrive still arrives, and the action still succeeded.
    assert_eq!(
        spawned.branch.resources["db"].resolved_exports["ANON_KEY"],
        "abc"
    );
    assert_eq!(spawned.branch.resources["db"].status, ResourceStatus::Ready);

    // What did not arrive is named, once, with the log to look in.
    assert_eq!(resource.missing_captures.len(), 1);
    let warning = &resource.missing_captures[0];
    assert!(warning.contains("SERVICE_ROLE_KEY"), "names the capture");
    assert!(!warning.contains("ANON_KEY"), "not the one that arrived");
    assert!(warning.contains("db.prepare"), "points at the log");

    // And on a later `newgit action`, not just at spawn.
    let outcome = manager
        .run_action("feature-a", "db.prepare")
        .expect("re-run");
    let ActionOutcome::Ran {
        missing_captures, ..
    } = outcome
    else {
        panic!("expected a one-shot run");
    };
    assert_eq!(missing_captures.len(), 1);
    assert!(missing_captures[0].contains("SERVICE_ROLE_KEY"));
}

const WORKDIR_RESOURCE: &str = r#"ownership = "workspace"
workdir = "packages/db"

[identity]
paths = ["packages/db/marker.txt"]

[actions.prepare]
command = "pwd > prepare-cwd.txt"

[actions.migrate]
workdir = "packages/db/supabase"
command = "pwd > migrate-cwd.txt"
"#;

/// Both directories a `workdir` might point at have to exist in the
/// workspace before the command that uses them runs; committing them into
/// the store repo means the clone that becomes the workspace already has
/// them, without newgit having to create them itself.
fn commit_dir(repo: &Utf8Path, path: &str) {
    let dir = repo.join(path);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(".keep"), "").expect("write");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", &format!("add {path}")]);
}

#[test]
fn workdir_runs_the_command_there_instead_of_a_cd_prefix() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    commit_dir(&repo, "packages/db/supabase");
    std::fs::write(repo.join("packages/db/marker.txt"), "v1\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "add marker"]);

    write_resource(&store, "db", WORKDIR_RESOURCE);
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let ws = spawned.branch.workspace_path.clone();

    // `prepare` has no workdir of its own, so it inherits the resource-level
    // one: it ran from `packages/db`, not the workspace root.
    assert_eq!(
        std::fs::read_to_string(ws.join("packages/db/prepare-cwd.txt")).expect("read"),
        format!("{}\n", ws.join("packages/db"))
    );

    // `migrate` declares its own workdir, which replaces the resource-level
    // one rather than nesting under it.
    manager
        .run_action("feature-a", "db.migrate")
        .expect("run migrate");
    assert_eq!(
        std::fs::read_to_string(ws.join("packages/db/supabase/migrate-cwd.txt")).expect("read"),
        format!("{}\n", ws.join("packages/db/supabase"))
    );

    // `[identity].paths` stayed workspace-root-relative: `newgit` found the
    // file at `packages/db/marker.txt`, not `packages/db/packages/db/marker.txt`.
    let outcome = manager.checkpoint("feature-a", None);
    assert!(
        outcome.is_ok(),
        "checkpoint should resolve identity paths against the workspace root: {outcome:?}"
    );
}

const MISSING_WORKDIR_RESOURCE: &str = r#"ownership = "workspace"
workdir = "packages/db"

[actions.prepare]
command = "true"
"#;

#[test]
fn a_workdir_that_does_not_exist_fails_with_a_clear_error() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", MISSING_WORKDIR_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    // `packages/db` was never created or committed, so `prepare` cannot run
    // there. The whole spawn aborts, the same way a missing binary would.
    let err = manager
        .spawn("feature-a", None)
        .expect_err("spawn should fail");
    match err {
        NewgitError::MissingWorkdir {
            resource,
            action,
            path,
        } => {
            assert_eq!(resource, "db");
            assert_eq!(action, "prepare");
            assert!(
                path.ends_with("packages/db"),
                "path should name the resolved workdir, was {path}"
            );
        }
        other => panic!("expected MissingWorkdir, got {other:?}"),
    }
}

#[test]
fn remove_resource_names_the_valid_ones_when_asked_for_an_unknown_name() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "prep", PREP_RESOURCE);
    let manager = BranchManager::open(MetadataStore::at(store.paths().project_root.clone()))
        .expect("manager");

    let err = manager
        .remove_resource("nope", false)
        .expect_err("unknown resource");
    assert!(err.to_string().contains("prep"));
}

#[test]
fn remove_resource_refuses_a_dependent_even_with_force() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", APP_RESOURCE);
    write_resource(&store, "prep", PREP_RESOURCE);
    let manager = BranchManager::open(MetadataStore::at(store.paths().project_root.clone()))
        .expect("manager");

    // `app` depends on `prep`; `--force` only answers "what about a bound
    // instance", never "what about the rest of the graph".
    for force in [false, true] {
        let err = manager
            .remove_resource("prep", force)
            .expect_err("still depended on");
        assert!(err.to_string().contains("app"), "{err}");
    }
    assert_eq!(manager.resource_definitions().len(), 2);
}

#[test]
fn remove_resource_refuses_a_bound_instance_without_force_and_drops_it_with() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "prep", PREP_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("manager");
    manager.spawn("feature-a", None).expect("spawn");

    let err = manager
        .remove_resource("prep", false)
        .expect_err("bound instance");
    assert!(err.to_string().contains("feature-a"));
    assert_eq!(manager.resource_definitions().len(), 1);

    let outcome = manager.remove_resource("prep", true).expect("force remove");
    assert!(!outcome.path.exists(), "definition file deleted");
    assert_eq!(outcome.unbound_instances, vec!["feature-a".to_owned()]);

    let branch = manager
        .store()
        .find_branch("feature-a")
        .expect("reload branch");
    assert!(
        !branch.resources.contains_key("prep"),
        "the binding is dropped, which is how a port is \"released\": there is no other ledger"
    );

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    assert!(manager.resource_definitions().is_empty());
}
