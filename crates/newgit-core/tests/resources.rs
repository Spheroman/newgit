use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::branch::ResourceStatus;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::config::WorkspaceSection;
use newgit_core::installs::{CopyMethod, InstallReport};
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

/// A data edge does not block its reader when the export's owner fails to
/// prepare, and it does not need to. The two halves below are the whole
/// argument, and they are worth pinning because a regression in either would
/// be silent.
///
/// The distinction is what a static `[exports]` value can be made of: ports,
/// branch vars, workspace, scripts, and other exports — none of which depend
/// on `prepare` succeeding. Anything that *does* must arrive through
/// `captures`, which are absent when prepare fails, and absence refuses.
///
/// So blocking on a data edge would be actively wrong: it would withhold a
/// correct render because an unrelated process failed to start, which is the
/// over-claiming #43 exists to remove.
#[test]
fn a_failed_export_owner_refuses_its_reader_only_when_the_value_is_actually_missing() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    // The owner's prepare fails. `TUNNEL_URL` is captured, so it never
    // exists; `WEB_URL` is static, so it exists regardless.
    write_resource(
        &store,
        "owner",
        r#"ownership = "branch"

[ports]
app = { start = 4100 }

[exports]
WEB_URL = "http://127.0.0.1:{{ports.app}}"

[actions.prepare]
command = "exit 1"
captures = ["TUNNEL_URL"]
"#,
    );
    // Reads the value that does exist. Declares no dependency.
    write_resource(
        &store,
        "reads-static",
        r#"ownership = "branch"

[exports]
HEALTH_URL = "{{exports.WEB_URL}}/health"

[actions.prepare]
command = "touch reads-static.txt"
"#,
    );
    // Reads the value that does not.
    write_resource(
        &store,
        "reads-captured",
        r#"ownership = "branch"

[exports]
PROBE_URL = "{{exports.TUNNEL_URL}}/health"

[actions.prepare]
command = "touch reads-captured.txt"
"#,
    );

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let resources = &spawned.branch.resources;
    assert_eq!(resources["owner"].status, ResourceStatus::Failed);

    // The port was allocated to this instance whether or not the process
    // came up, so the value is correct, not stale. The reader proceeds.
    assert_eq!(
        resources["reads-static"].resolved_exports["HEALTH_URL"],
        format!(
            "http://127.0.0.1:{}/health",
            resources["owner"].resolved_ports["app"]
        )
    );
    assert_eq!(resources["reads-static"].status, ResourceStatus::Ready);
    assert!(
        spawned
            .branch
            .workspace_path
            .join("reads-static.txt")
            .is_file(),
        "a correct value is not withheld because an unrelated process failed"
    );

    // The captured value never arrived, so the reader refuses loudly and its
    // prepare never runs — without any blocking rule saying so.
    assert_eq!(resources["reads-captured"].status, ResourceStatus::Failed);
    assert!(
        resources["reads-captured"].resolved_exports.is_empty(),
        "nothing half-resolved is stored"
    );
    assert!(
        !spawned
            .branch
            .workspace_path
            .join("reads-captured.txt")
            .exists(),
        "a missing value stops the reader on its own"
    );
}

/// Adding the same template twice — a web and an api — is the canonical
/// setup, and the one-owner rule turns a template that ships conventional
/// names into a graph that refuses on the second `resource add`. The names
/// are derived from the resource instead, so the obvious first thing a user
/// does keeps working.
#[test]
fn adding_one_template_twice_leaves_a_graph_that_still_spawns() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    for name in ["api", "web"] {
        BranchManager::open(MetadataStore::at(repo.clone()))
            .expect("manager")
            .add_resource(name, "process")
            .expect("add");
    }

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    assert!(
        manager.graph_problems().is_empty(),
        "no collision: {:?}",
        manager.graph_problems()
    );

    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let env: std::collections::BTreeMap<_, _> = manager
        .assemble_env(&spawned.branch)
        .expect("env")
        .into_iter()
        .collect();

    // Both services reach the same environment with their own names, which is
    // the thing a single `PORT` could never express.
    assert_ne!(env["WEB_PORT"], env["API_PORT"]);
    assert!(env["WEB_URL"].ends_with(&env["WEB_PORT"]));
    assert!(env["API_URL"].ends_with(&env["API_PORT"]));
}

/// Two resources exporting one name used to resolve to whichever bound last,
/// so the loser was absent from `newgit run` with nothing anywhere saying
/// why. It is now a graph problem, reported and gated like any other.
#[test]
fn two_resources_exporting_one_name_block_graph_commands() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    for name in ["metro", "supabase"] {
        write_resource(
            &store,
            name,
            r#"ownership = "branch"

[exports]
EXPO_URL = "exp://127.0.0.1:8081"
"#,
        );
    }

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("open still works");
    assert!(matches!(
        manager.graph_problems(),
        [newgit_core::resource::GraphProblem::EnvNameCollision { name, .. }]
            if name == "EXPO_URL"
    ));

    let error = manager.spawn("blocked", None).expect_err("spawn refuses");
    let message = error.to_string();
    // The message has to name both claimants: either one alone is a rename
    // you cannot evaluate without knowing what it is colliding with.
    assert!(
        message.contains("`metro` [exports]") && message.contains("`supabase` [exports]"),
        "both claimants named: {message}"
    );

    // Listing definitions is how you find the collision, so it must not refuse.
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

const API_RESOURCE: &str = r#"ownership = "branch"

[ports]
api = { start = 54321 }

[exports]
API_URL = "http://127.0.0.1:{{ports.api}}"
"#;

const COMPOSED_EXPORT_RESOURCE: &str = r#"ownership = "branch"
depends_on = ["api"]

[exports]
FUNCTIONS_URL = "{{exports.API_URL}}/functions/v1"
"#;

/// A `[[render]]` could already compose a dependency's export, so a *file*
/// could carry another resource's URL while the resource that produces those
/// values could not. Bindings run in dependency order, so the value is there.
#[test]
fn an_export_may_compose_a_dependencys_export() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "api", API_RESOURCE);
    write_resource(&store, "functions", COMPOSED_EXPORT_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let outcome = manager.spawn("feature-a", None).expect("spawn");
    let api_port = outcome.branch.resources["api"].resolved_ports["api"];
    let functions = outcome
        .branch
        .resources
        .get("functions")
        .expect("functions bound");
    // The allocator may not hand out `start` verbatim (e.g. a concurrent
    // test in the same binary can already hold it), so assert against the
    // port this instance actually resolved rather than a literal — the
    // claim under test is that the export composes the dependency's export,
    // not which port the allocator picked.
    assert_eq!(
        functions
            .resolved_exports
            .get("FUNCTIONS_URL")
            .map(String::as_str),
        Some(format!("http://127.0.0.1:{api_port}/functions/v1").as_str())
    );
}

/// `HEALTH_URL` sorts *before* `BASE_URL`, so nothing about declaration order
/// could have saved this one: the table is a map.
const SIBLING_EXPORT_RESOURCE: &str = r#"ownership = "branch"

[ports]
app = { start = 4100 }

[exports]
BASE_URL = "http://127.0.0.1:{{ports.app}}"
HEALTH_URL = "{{exports.BASE_URL}}/health"
"#;

/// Composing a sibling is the obvious thing to write, and refusing it while
/// accepting the identical line pointed at a *dependency* would be a rule
/// nobody could guess — worse, the error named a key defined two lines above.
#[test]
fn an_export_may_compose_a_sibling_whatever_the_key_order() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", SIBLING_EXPORT_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let outcome = manager.spawn("feature-a", None).expect("spawn");
    let app = outcome.branch.resources.get("app").expect("app bound");
    let app_port = app.resolved_ports["app"];
    // See the comment on `an_export_may_compose_a_dependencys_export`: the
    // allocator's actual pick, not the literal `start`, since what's under
    // test here is composition, not the number itself.
    assert_eq!(
        app.resolved_exports.get("HEALTH_URL").map(String::as_str),
        Some(format!("http://127.0.0.1:{app_port}/health").as_str())
    );
}

const CYCLIC_EXPORT_RESOURCE: &str = r#"ownership = "branch"

[exports]
A = "{{exports.B}}"
B = "{{exports.A}}"
"#;

/// Resolving to a fixed point must stall on a cycle rather than spin, and
/// report it like any other placeholder that never resolved.
#[test]
fn exports_that_reference_each_other_stall_rather_than_loop() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "loop", CYCLIC_EXPORT_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let outcome = manager.spawn("feature-a", None).expect("spawn");
    let report = outcome
        .resources
        .iter()
        .find(|resource| resource.name == "loop")
        .expect("loop reported");
    assert_eq!(report.status, ResourceStatus::Failed);
    let error = report.export_error.as_deref().expect("export error");
    assert!(error.contains("`A`") && error.contains("`B`"), "{error}");
}

const PARTIAL_EXPORT_RESOURCE: &str = r#"ownership = "branch"

[ports]
app = { start = 4200 }

[exports]
GOOD_URL = "http://127.0.0.1:{{ports.app}}"
BAD_URL = "http://127.0.0.1:{{ports.nope}}"
ALSO_BAD = "{{ports.also_nope}}"
"#;

/// A binding that publishes half an environment is the case the refusal
/// exists to prevent: `newgit run` and every dependent's actions read a
/// binding's exports without asking what status it holds.
#[test]
fn one_unresolved_export_withholds_every_export_of_that_resource() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", PARTIAL_EXPORT_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let outcome = manager.spawn("feature-a", None).expect("spawn");
    let report = outcome
        .resources
        .iter()
        .find(|resource| resource.name == "app")
        .expect("app reported");
    assert_eq!(report.status, ResourceStatus::Failed);

    let error = report.export_error.as_deref().expect("export error");
    assert!(
        error.contains("BAD_URL") && error.contains("ALSO_BAD"),
        "every unresolved export is named, not just the first: {error}"
    );

    let binding = outcome.branch.resources.get("app").expect("app bound");
    assert!(
        binding.resolved_exports.is_empty(),
        "a resolved sibling is still half an environment: {:?}",
        binding.resolved_exports
    );
}

const UNRESOLVED_EXPORT_RESOURCE: &str = r#"ownership = "branch"
depends_on = ["api"]

[exports]
FUNCTIONS_URL = "http://127.0.0.1:{{ports.api.nope}}/functions/v1"
"#;

/// Verbatim is right where the mistake surfaces in front of whoever typed it.
/// An export is written to the record once and handed to every later process,
/// so it refuses instead — and the bad value is never stored, because an
/// absent variable is detectable and a malformed URL is not.
#[test]
fn an_unresolved_export_refuses_and_is_not_stored() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "api", API_RESOURCE);
    write_resource(&store, "functions", UNRESOLVED_EXPORT_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let outcome = manager.spawn("feature-b", None).expect("spawn");
    let report = outcome
        .resources
        .iter()
        .find(|resource| resource.name == "functions")
        .expect("functions reported");
    assert_eq!(report.status, ResourceStatus::Failed);
    let error = report.export_error.as_deref().expect("export error");
    assert!(error.contains("FUNCTIONS_URL"), "names the export: {error}");
    assert!(
        error.contains("{{ports.api.nope}}"),
        "names the placeholder: {error}"
    );
    assert!(
        report.render_error.is_none(),
        "an unresolved export is not a render failure"
    );

    let binding = outcome
        .branch
        .resources
        .get("functions")
        .expect("functions bound");
    assert!(
        !binding.resolved_exports.contains_key("FUNCTIONS_URL"),
        "the literal is not stored: {:?}",
        binding.resolved_exports
    );
}

/// The point of the install store, end to end: the second instance of the
/// same lockfile does not install.
#[test]
fn a_second_instance_of_the_same_identity_is_filled_rather_than_installed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let runs = temp.join("runs.txt");

    // The identity has to be committed: a workspace is a fresh clone, so an
    // uncommitted lockfile never reaches the instance that keys on it.
    std::fs::write(repo.join("lock.txt"), "v1\n").expect("write lock");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "lockfile"]);

    write_resource(
        &store,
        "deps",
        &format!(
            r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/marker && echo ran >> {runs}"
"#
        ),
    );
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("manager");

    let first = manager.spawn("one", None).expect("spawn one");
    let report = first
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps reported");
    assert_eq!(report.status, ResourceStatus::Ready);
    assert!(
        report.prepare.is_some(),
        "the first instance of an identity installs"
    );
    let key = match report.install.as_ref().expect("an install report") {
        InstallReport::Stored { key, .. } => key.clone(),
        other => panic!("the first install should be published: {other:?}"),
    };
    assert_eq!(
        std::fs::read_to_string(&runs).expect("runs"),
        "ran\n",
        "installed exactly once"
    );

    let second = manager.spawn("two", None).expect("spawn two");
    let report = second
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps reported");
    assert_eq!(report.status, ResourceStatus::Ready);
    assert!(
        report.prepare.is_none(),
        "the second instance must not run the install at all"
    );
    // The copy method is whatever this filesystem can do — asserting
    // `Reflink` would pass on APFS and fail in a container on overlayfs,
    // where the behaviour is identical and only the cost differs.
    match report.install.as_ref().expect("an install report") {
        InstallReport::Filled {
            key: filled,
            method,
        } => {
            assert_eq!(filled, &key, "from the entry the first instance stored");
            assert!(matches!(method, CopyMethod::Reflink | CopyMethod::Full));
        }
        other => panic!("the second instance should be filled: {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&runs).expect("runs"),
        "ran\n",
        "and the install command is not run a second time"
    );
    assert_eq!(
        std::fs::read_to_string(second.branch.workspace_path.join("node_modules/marker"))
            .expect("filled tree"),
        "built\n",
        "but the tree is there all the same"
    );

    // A moved lockfile is a different key, and the whole premise is that it
    // rebuilds rather than reusing a tree built from the old one.
    std::fs::write(repo.join("lock.txt"), "v2\n").expect("bump lock");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "bump"]);

    let third = manager.spawn("three", None).expect("spawn three");
    let report = third
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps reported");
    assert!(
        report.prepare.is_some(),
        "a different lockfile must install, not reuse"
    );
    match report.install.as_ref().expect("an install report") {
        InstallReport::Stored { key: bumped, .. } => {
            assert_ne!(bumped, &key, "and is stored under its own key")
        }
        other => panic!("expected a second entry: {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&runs).expect("runs"),
        "ran\nran\n",
        "exactly one more install"
    );
}

/// `key_command` is the escape hatch for what a lockfile cannot describe:
/// same inputs, different toolchain, different tree.
#[test]
fn key_command_output_separates_two_trees_built_from_one_lockfile() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let toolchain = temp.join("toolchain.txt");
    std::fs::write(&toolchain, "v24\n").expect("write toolchain");

    std::fs::write(repo.join("lock.txt"), "v1\n").expect("write lock");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "lockfile"]);

    write_resource(
        &store,
        "deps",
        &format!(
            r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]
key_command = "cat {toolchain}"

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/marker"
"#
        ),
    );
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("manager");

    let first = manager.spawn("one", None).expect("spawn one");
    let first_key = install_key(&first.resources);

    // Same lockfile, different toolchain: the tree from the first is not
    // interchangeable with what this one needs, so the key has to move.
    std::fs::write(&toolchain, "v22\n").expect("bump toolchain");
    let second = manager.spawn("two", None).expect("spawn two");
    let report = second
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps reported");
    assert!(
        report.prepare.is_some(),
        "a toolchain bump must rebuild even though the lockfile did not move"
    );
    assert_ne!(install_key(&second.resources), first_key);
}

fn install_key(resources: &[newgit_core::manager::ResourceBindOutcome]) -> String {
    match resources
        .iter()
        .find(|resource| resource.name == "deps")
        .and_then(|resource| resource.install.as_ref())
        .expect("an install report")
    {
        InstallReport::Stored { key, .. } | InstallReport::Filled { key, .. } => key.clone(),
        other => panic!("expected a keyed report: {other:?}"),
    }
}

/// The store is a cache, and a cache that only grows is a disk leak. An
/// entry survives exactly as long as some instance still keys to it.
#[test]
fn cleanup_drops_install_trees_no_instance_keys_to_any_more() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    std::fs::write(repo.join("lock.txt"), "v1\n").expect("write lock");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "lockfile"]);

    write_resource(
        &store,
        "deps",
        r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/marker"
"#,
    );
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("manager");
    let first = manager.spawn("one", None).expect("spawn one");
    let old_key = install_key(&first.resources);

    // The lockfile moves on, and a new instance builds a new tree. The old
    // entry now belongs to nobody: `one` is still live, but it keys to the
    // lockfile in *its* workspace, which is the one it was spawned with.
    std::fs::write(repo.join("lock.txt"), "v2\n").expect("bump lock");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "bump"]);
    let second = manager.spawn("two", None).expect("spawn two");
    let new_key = install_key(&second.resources);
    assert_ne!(old_key, new_key);

    // Both instances are live and each keys to its own entry, so a cleanup
    // here must remove neither.
    let untouched = manager
        .cleanup(true, ArchivedCheckpoints::Keep)
        .expect("dry run");
    assert!(
        untouched.pruned_installs.is_empty(),
        "an entry a live instance keys to is not garbage: {:?}",
        untouched.pruned_installs
    );

    // Retire the instance holding the old key, and it becomes unreachable.
    manager
        .remove("one", &repo, ArchivedCheckpoints::Keep)
        .expect("remove one");
    let outcome = manager
        .cleanup(false, ArchivedCheckpoints::Keep)
        .expect("cleanup");
    let pruned: Vec<&str> = outcome
        .pruned_installs
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(
        pruned,
        vec![old_key.as_str()],
        "exactly the entry nothing keys to"
    );
    assert!(
        !outcome.pruned_installs[0].path.exists(),
        "and it is actually gone from disk"
    );

    // The surviving instance's tree is untouched, so it still spawns free.
    let third = manager.spawn("three", None).expect("spawn three");
    let report = third
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps reported");
    assert!(
        report.prepare.is_none(),
        "the surviving entry still fills a new instance"
    );
}

/// The command that builds a tree is as much an input as the lockfile it
/// reads. Without the definition in the key, editing `prepare` would leave
/// the next instance filled from the tree the *old* command built.
#[test]
fn editing_the_producing_command_invalidates_the_stored_tree() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    std::fs::write(repo.join("lock.txt"), "v1\n").expect("write lock");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "lockfile"]);

    let definition = |flavour: &str| {
        format!(
            r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]

[actions.prepare]
command = "mkdir -p node_modules && echo {flavour} > node_modules/flavour"
"#
        )
    };
    write_resource(&store, "deps", &definition("full"));
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("manager");
    let first = manager.spawn("one", None).expect("spawn one");
    let full_key = install_key(&first.resources);

    // Same lockfile, different command. The tree it builds is different, so
    // the key has to be.
    write_resource(&store, "deps", &definition("slim"));
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("reopen");
    let second = manager.spawn("two", None).expect("spawn two");
    let report = second
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps reported");
    assert!(
        report.prepare.is_some(),
        "an edited producing command must run, not be served from the old tree"
    );
    assert_ne!(install_key(&second.resources), full_key);
    assert_eq!(
        std::fs::read_to_string(second.branch.workspace_path.join("node_modules/flavour"))
            .expect("read"),
        "slim\n"
    );
}

/// `--dry-run` is the command people reach for because it observes without
/// acting. Spawning a user-supplied shell is acting.
#[test]
fn a_dry_run_cleanup_never_runs_a_key_command() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let ran = temp.join("key-command-ran.txt");

    std::fs::write(repo.join("lock.txt"), "v1\n").expect("write lock");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "lockfile"]);

    write_resource(
        &store,
        "deps",
        &format!(
            r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]
key_command = "echo ran >> {ran}; echo v24"

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/marker"
"#
        ),
    );
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("manager");
    manager.spawn("one", None).expect("spawn one");
    let before = std::fs::read_to_string(&ran).expect("ran").lines().count();

    let outcome = manager
        .cleanup(true, ArchivedCheckpoints::Keep)
        .expect("dry run");
    assert_eq!(
        std::fs::read_to_string(&ran).expect("ran").lines().count(),
        before,
        "a dry run must not spawn the resource's key_command"
    );
    assert!(
        outcome.pruned_installs.is_empty(),
        "and must not claim it would prune what it could not key"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("--dry-run runs no key_command")),
        "it says so instead: {:?}",
        outcome.warnings
    );
}

/// One instance that cannot be keyed must not disable pruning for every
/// other resource — that turns a cache into an unbounded disk leak.
#[test]
fn an_unkeyable_resource_withholds_only_its_own_entries() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    std::fs::write(repo.join("lock.txt"), "v1\n").expect("write lock");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "lockfile"]);

    write_resource(
        &store,
        "deps",
        r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/marker"
"#,
    );
    write_resource(
        &store,
        "vendor",
        r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["vendor"]
key_command = "exit 3"

[actions.prepare]
command = "mkdir -p vendor && echo built > vendor/marker"
"#,
    );
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("manager");
    let spawned = manager.spawn("one", None).expect("spawn one");

    // `deps` has an entry nothing keys to once its lockfile moves on;
    // `vendor` cannot be keyed at all.
    std::fs::write(spawned.branch.workspace_path.join("lock.txt"), "v9\n").expect("bump lock");

    let outcome = manager
        .cleanup(false, ArchivedCheckpoints::Keep)
        .expect("cleanup");
    let pruned: Vec<&str> = outcome
        .pruned_installs
        .iter()
        .map(|entry| entry.resource.as_str())
        .collect();
    assert_eq!(
        pruned,
        vec!["deps"],
        "the keyable resource is still swept: {:?}",
        outcome.pruned_installs
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("`vendor`")),
        "and the one that was left alone is named: {:?}",
        outcome.warnings
    );
}
