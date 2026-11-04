//! Changing a live instance's binding rebuilds the installs keyed on it.
//!
//! Since the install store keys a tree on the trackers its instance binds
//! (`install_key_bound_set.rs`), a tree built under one binding is the wrong
//! tree under another: a shared install names its members by
//! `{{bound.paths}}`, or finds them on disk, where a dropped tracker's files
//! are not checked out. Two paths changed the binding of a live instance and
//! left such a tree as it was: `tracker use` / `tracker drop`, which only
//! started and stopped the resources the tracker brings, and `undo`, whose
//! `recompute` skip compared the identity hash alone, which a binding is not
//! part of.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::ResourceStatus;
use newgit_core::config::WorkspaceSection;
use newgit_core::installs::InstallReport;
use newgit_core::manager::{BindingChoice, BranchManager, Uncommitted, UndoOptions, UndoOutcome};
use newgit_core::store::MetadataStore;

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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A workspace-style monorepo: one root lockfile listing two members, each
/// member carved into its own code tracker, and a shared install keyed on
/// the lockfile whose `prepare` builds the bound members, named by
/// `{{bound.paths}}`. It checkpoints by identity hash and restores by
/// recomputing `prepare`, as `node_modules` would.
fn setup(temp: &Utf8Path) -> BranchManager {
    setup_with(temp, &["a", "b"])
}

/// [`setup`], with one member and code tracker per name.
fn setup_with(temp: &Utf8Path, names: &[&str]) -> BranchManager {
    let repo = temp.join("store");
    git_init(&repo);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    let mut lock = String::new();
    for name in names {
        std::fs::create_dir_all(repo.join("apps").join(name)).expect("mkdir");
        std::fs::write(
            repo.join(format!("apps/{name}/index.js")),
            format!("{name}\n"),
        )
        .expect("write");
        lock.push_str(&format!("{name}@1\n"));
    }
    std::fs::write(repo.join("lock.txt"), lock).expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);

    for name in names {
        BranchManager::open(store.clone())
            .expect("manager")
            .create_code_tracker(
                name,
                "project-devs",
                &[Utf8PathBuf::from(format!("apps/{name}"))],
            )
            .expect("carve");
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    let manager = BranchManager::open(store).expect("manager");
    // The project root's own commits reach `main` like anyone's.
    manager
        .merge("main", "main")
        .expect("merge the project root");

    define_resource(&manager, "deps", &deps_toml(""))
}

fn git_init(repo: &Utf8Path) {
    std::fs::create_dir_all(repo).expect("mkdir");
    git(repo, &["init", "-q", "-b", "main"]);
}

/// The shared install, with `before` run ahead of building the tree. Every
/// time `prepare` really runs it appends a line to `runs.txt`, so a test can
/// count rebuilds that the install store did not fill.
fn deps_toml(before: &str) -> String {
    format!(
        r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]

[actions.prepare]
command = "echo run >> runs.txt && {before} mkdir -p node_modules && echo '{{{{bound.paths}}}}' > node_modules/members.txt"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#
    )
}

/// Write (or replace) a resource definition and reopen the manager on it.
fn define_resource(manager: &BranchManager, name: &str, toml: &str) -> BranchManager {
    std::fs::write(
        manager
            .store()
            .paths()
            .resources
            .join(format!("{name}.toml")),
        toml,
    )
    .expect("resource");
    BranchManager::open(manager.store().clone()).expect("reopen")
}

/// Add lines to a code tracker's definition and reopen the manager on it.
fn extend_tracker(manager: &BranchManager, name: &str, lines: &str) -> BranchManager {
    let path = manager
        .store()
        .paths()
        .trackers
        .join(format!("{name}.toml"));
    let mut toml = std::fs::read_to_string(&path).expect("tracker definition");
    toml.push_str(lines);
    std::fs::write(&path, toml).expect("tracker definition");
    BranchManager::open(manager.store().clone()).expect("reopen")
}

/// How many times `deps`'s `prepare` really ran in `workspace`.
fn runs(workspace: &Utf8Path) -> usize {
    std::fs::read_to_string(workspace.join("runs.txt"))
        .map(|runs| runs.lines().count())
        .unwrap_or(0)
}

fn status(manager: &BranchManager, instance: &str, resource: &str) -> ResourceStatus {
    manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .resources[resource]
        .status
}

fn deps_action(outcome: &UndoOutcome) -> String {
    outcome
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps restored")
        .action
        .clone()
}

fn drop_b() -> BindingChoice {
    drops(&["b"])
}

fn drops(names: &[&str]) -> BindingChoice {
    BindingChoice {
        drops: names.iter().map(|name| (*name).to_owned()).collect(),
        ..Default::default()
    }
}

fn undo_to(id: &str) -> UndoOptions {
    UndoOptions {
        to: Some(id.to_owned()),
        ..Default::default()
    }
}

fn members(workspace: &Utf8Path) -> Vec<String> {
    let mut members: Vec<String> =
        std::fs::read_to_string(workspace.join("node_modules/members.txt"))
            .expect("the install's tree")
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect();
    members.sort();
    members
}

#[test]
fn tracker_use_rebuilds_the_install_for_the_wider_binding() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    assert_eq!(members(&workspace), ["apps/a"], "precondition: `b` dropped");

    let change = manager.use_tracker("x", "b").expect("use b");

    assert!(
        workspace.join("apps/b/index.js").exists(),
        "precondition: `b` is checked out"
    );
    assert_eq!(
        members(&workspace),
        ["apps/a", "apps/b"],
        "`x` now binds `b`, but holds the tree built without it"
    );
    let rebuilt: Vec<&str> = change
        .rebuilt
        .iter()
        .map(|install| install.name.as_str())
        .collect();
    assert_eq!(rebuilt, ["deps"], "the change says what it rebuilt");
    assert!(change.rebuilt[0].ok, "{:?}", change.rebuilt[0]);
}

#[test]
fn tracker_drop_rebuilds_the_install_for_the_narrower_binding() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager.spawn("x", None).expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    assert_eq!(members(&workspace), ["apps/a", "apps/b"], "precondition");

    let change = manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");

    assert_eq!(
        members(&workspace),
        ["apps/a"],
        "`x` dropped `b`, but holds the tree built with it"
    );
    assert_eq!(change.rebuilt.len(), 1, "{:?}", change.rebuilt);
    assert_eq!(change.rebuilt[0].name, "deps");
}

/// The rebuild is the same job a spawn does, so it goes through the install
/// store: an instance that already built the tree for the new binding has
/// stored it, and the change is filled from that entry rather than running
/// the install again.
#[test]
fn the_rebuild_is_filled_from_the_store_when_that_binding_was_built_before() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    manager.spawn("both", None).expect("spawn both");
    manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");

    let change = manager.use_tracker("x", "b").expect("use b");

    match &change.rebuilt[..] {
        [install] => assert!(
            matches!(install.install, Some(InstallReport::Filled { .. })),
            "the tree `both` stored is the tree `x` now needs: {install:?}"
        ),
        other => panic!("one rebuild expected: {other:?}"),
    }
    let workspace = manager.store().find_branch("x").expect("x").workspace_path;
    assert_eq!(members(&workspace), ["apps/a", "apps/b"]);
}

/// A resource that declares no `[identity] produces` holds no tree the
/// binding could have keyed, so a binding change leaves it alone.
#[test]
fn a_resource_with_no_produced_tree_is_not_rebuilt() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    std::fs::write(
        manager.store().paths().resources.join("deps.toml"),
        r#"ownership = "workspace"

[actions.prepare]
command = "echo '{{bound.paths}}' > prepared.txt"
"#,
    )
    .expect("resource");
    let manager = BranchManager::open(manager.store().clone()).expect("reopen");
    let spawned = manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;

    let change = manager.use_tracker("x", "b").expect("use b");

    assert!(change.rebuilt.is_empty(), "{:?}", change.rebuilt);
    assert_eq!(
        std::fs::read_to_string(workspace.join("prepared.txt")).expect("prepared"),
        "apps/a\n"
    );
}

/// Undo's `recompute` skip compared the checkpoint's identity hash with the
/// pre-undo one. The binding is not in that hash, so an undo that restored a
/// different binding kept the tree built for the binding it replaced.
#[test]
fn undo_that_restores_a_different_binding_does_not_skip_the_recompute() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");

    manager.use_tracker("x", "b").expect("use b");
    // Built by hand for the wider binding, as the issue did, so the test
    // does not depend on `use` having rebuilt it.
    manager.run_action("x", "deps.prepare").expect("prepare");
    assert_eq!(
        members(&workspace),
        ["apps/a", "apps/b"],
        "precondition: built for the wider binding"
    );

    let outcome = manager
        .undo(
            "x",
            &UndoOptions {
                to: Some(checkpoint.record.id.clone()),
                ..Default::default()
            },
        )
        .expect("undo");

    assert!(
        !workspace.join("apps/b/index.js").exists(),
        "precondition: undo dropped `b` again"
    );
    let deps = outcome
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps restored");
    assert!(
        !deps.action.contains("skipped"),
        "the binding moved, so the identity is not unchanged: {}",
        deps.action
    );
    assert_eq!(
        members(&workspace),
        ["apps/a"],
        "`x` binds only `a` again, but holds the tree built with `b`"
    );
}

/// The skip is still a skip when only the lockfile is unchanged *and* the
/// binding is: that is the case it exists for, and the one a full reinstall
/// in the middle of an undo would be paid for nothing.
#[test]
fn undo_to_the_same_binding_still_skips_the_recompute() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    std::fs::write(workspace.join("README.md"), "changed\n").expect("write");

    let outcome = manager
        .undo(
            "x",
            &UndoOptions {
                to: Some(checkpoint.record.id.clone()),
                ..Default::default()
            },
        )
        .expect("undo");

    let deps = outcome
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps restored");
    assert!(deps.action.contains("skipped"), "{}", deps.action);
}

/// Undo's skip ignores the binding only for a resource the binding keys: one
/// that declares `[identity] produces`. A codegen step keyed on its inputs
/// alone has no tree in the store, and nothing the binding changes, so an
/// undo that restored another binding still skips it when its inputs match.
#[test]
fn undo_still_skips_a_recompute_with_no_produced_tree_when_the_binding_moves() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let manager = define_resource(
        &manager,
        "codegen",
        r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]

[actions.prepare]
command = "echo generated > generated.txt"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#,
    );
    manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    manager.use_tracker("x", "b").expect("use b");

    let outcome = manager
        .undo("x", &undo_to(&checkpoint.record.id))
        .expect("undo");

    let codegen = outcome
        .resources
        .iter()
        .find(|resource| resource.name == "codegen")
        .expect("codegen restored");
    assert!(codegen.action.contains("skipped"), "{}", codegen.action);
    assert!(
        !deps_action(&outcome).contains("skipped"),
        "the install the binding keys is still rebuilt: {}",
        deps_action(&outcome)
    );
}

/// A `use` that swaps out the tracker filling its role drops that one on
/// the way; the binding moves twice, but the install is rebuilt once, for
/// the binding the command ends with.
#[test]
fn a_role_swap_rebuilds_once() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let manager = extend_tracker(&manager, "a", "role = \"app\"\npriority = 1\n");
    let manager = extend_tracker(&manager, "b", "role = \"app\"\n");
    let spawned = manager.spawn("x", None).expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    assert_eq!(
        members(&workspace),
        ["apps/a"],
        "precondition: one per role"
    );
    assert_eq!(runs(&workspace), 1);

    let change = manager.use_tracker("x", "b").expect("use b");

    assert_eq!(change.replaced, ["a"]);
    assert_eq!(members(&workspace), ["apps/b"]);
    assert_eq!(change.rebuilt.len(), 1, "{:?}", change.rebuilt);
    assert_eq!(runs(&workspace), 2, "one rebuild, not one per step");
}

/// A `use` that brings what the tracker `requires` binds several trackers;
/// the install is rebuilt once, for all of them.
#[test]
fn a_use_that_brings_its_requirements_rebuilds_once() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let manager = extend_tracker(&manager, "a", "requires = [\"b\"]\n");
    let spawned = manager
        .spawn_binding("x", None, &drops(&["a", "b"]))
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    assert_eq!(runs(&workspace), 1);

    let change = manager.use_tracker("x", "a").expect("use a");

    assert_eq!(change.brought, ["b"]);
    assert_eq!(members(&workspace), ["apps/a", "apps/b"]);
    assert_eq!(change.rebuilt.len(), 1, "{:?}", change.rebuilt);
    assert_eq!(runs(&workspace), 2, "one rebuild, not one per tracker");
}

/// A resource one of the required trackers brings is bound by the same
/// `use`, so it is not rebuilt afterwards: it has to be built under the
/// binding the command ends with, not halfway through it.
#[test]
fn a_resource_a_requirement_brings_is_built_for_the_whole_binding() {
    let (_guard, temp) = tempdir();
    let manager = setup_with(&temp, &["a", "b", "c"]);
    let manager = extend_tracker(&manager, "a", "requires = [\"b\", \"c\"]\n");
    let manager = extend_tracker(&manager, "b", "resources = [\"bdeps\"]\n");
    let manager = define_resource(
        &manager,
        "bdeps",
        r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["b_modules"]

[actions.prepare]
command = "mkdir -p b_modules && echo '{{bound.paths}}' > b_modules/members.txt"
"#,
    );
    let spawned = manager
        .spawn_binding("x", None, &drops(&["a", "b", "c"]))
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;

    let change = manager.use_tracker("x", "a").expect("use a");

    assert_eq!(change.brought, ["b", "c"]);
    let mut built: Vec<String> = std::fs::read_to_string(workspace.join("b_modules/members.txt"))
        .expect("bdeps' tree")
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect();
    built.sort();
    assert_eq!(built, ["apps/a", "apps/b", "apps/c"]);
    assert!(change.started.contains(&"bdeps".to_owned()), "{change:?}");
}

/// A rebuild that fails leaves the resource `failed` with its log, says so,
/// and does not fail the command: the binding did change. What depends on
/// it is reported as not rebuilt rather than skipped silently, and left
/// `pending` behind it.
#[test]
fn a_failed_rebuild_is_reported_and_blocks_its_dependents() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    // Builds only while `b` is not checked out.
    let manager = define_resource(&manager, "deps", &deps_toml("test ! -d apps/b &&"));
    let manager = define_resource(
        &manager,
        "dev",
        r#"ownership = "workspace"
depends_on = ["deps"]

[identity]
paths = ["lock.txt"]
produces = ["dev_out"]

[actions.prepare]
command = "mkdir -p dev_out"
"#,
    );
    manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");
    assert_eq!(status(&manager, "x", "dev"), ResourceStatus::Ready);

    let change = manager.use_tracker("x", "b").expect("the binding changes");

    match &change.rebuilt[..] {
        [deps, dev] => {
            assert_eq!(deps.name, "deps");
            assert!(!deps.ok, "{deps:?}");
            let log = deps.log.as_ref().expect("the failed rebuild's log");
            assert!(log.exists(), "{log}");
            assert_eq!(dev.name, "dev");
            assert!(!dev.ok, "{dev:?}");
            assert_eq!(dev.blocked_by, ["deps"], "{dev:?}");
        }
        other => panic!("deps failed, dev blocked: {other:?}"),
    }
    assert_eq!(status(&manager, "x", "deps"), ResourceStatus::Failed);
    assert_eq!(status(&manager, "x", "dev"), ResourceStatus::Pending);
    assert!(
        !manager
            .store()
            .find_branch("x")
            .expect("x")
            .dropped
            .contains("b"),
        "`b` is bound"
    );
}

/// An error running the rebuild — not the command failing, newgit failing
/// to run it — is reported for that install like a failure, and the rest
/// still rebuild and are recorded. The binding has moved by then, so the
/// command reports that it did.
#[test]
fn an_error_running_a_rebuild_does_not_fail_the_binding_change() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    // Runs inside `b`, which a drop takes out of the working tree.
    let manager = define_resource(
        &manager,
        "inb",
        r#"ownership = "workspace"
workdir = "apps/b"

[identity]
paths = ["lock.txt"]
produces = ["b_out"]

[actions.prepare]
command = "mkdir -p ../../b_out"
"#,
    );
    let spawned = manager.spawn("x", None).expect("spawn x");
    let workspace = spawned.branch.workspace_path;

    let change = manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("the binding changes");

    let deps = change
        .rebuilt
        .iter()
        .find(|install| install.name == "deps")
        .expect("deps rebuilt");
    assert!(deps.ok, "{deps:?}");
    let inb = change
        .rebuilt
        .iter()
        .find(|install| install.name == "inb")
        .expect("inb reported");
    assert!(!inb.ok, "{inb:?}");
    assert!(
        inb.error
            .as_deref()
            .is_some_and(|error| error.contains("apps/b")),
        "{inb:?}"
    );
    assert_eq!(members(&workspace), ["apps/a"]);
    assert_eq!(status(&manager, "x", "deps"), ResourceStatus::Ready);
    assert_eq!(status(&manager, "x", "inb"), ResourceStatus::Failed);
    assert!(
        manager
            .store()
            .find_branch("x")
            .expect("x")
            .dropped
            .contains("b")
    );
}

/// An install that holds no tree yet — blocked behind a dependency that
/// never got ready, so `pending` — has nothing a binding made stale.
#[test]
fn a_pending_install_with_no_tree_is_not_rebuilt() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let manager = define_resource(&manager, "deps", &deps_toml("false &&"));
    let manager = define_resource(
        &manager,
        "dev",
        r#"ownership = "workspace"
depends_on = ["deps"]

[identity]
paths = ["lock.txt"]
produces = ["dev_out"]

[actions.prepare]
command = "mkdir -p dev_out"
"#,
    );
    let spawned = manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    assert_eq!(status(&manager, "x", "deps"), ResourceStatus::Failed);
    assert_eq!(status(&manager, "x", "dev"), ResourceStatus::Pending);

    let change = manager.use_tracker("x", "b").expect("use b");

    assert!(change.rebuilt.is_empty(), "{:?}", change.rebuilt);
    assert_eq!(runs(&workspace), 1, "the failed install was not retried");
    assert_eq!(status(&manager, "x", "dev"), ResourceStatus::Pending);
}

/// `use` of a tracker the instance already binds moves nothing.
#[test]
fn using_a_bound_tracker_rebuilds_nothing() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager.spawn("x", None).expect("spawn x");
    let workspace = spawned.branch.workspace_path;

    let change = manager.use_tracker("x", "b").expect("use b");

    assert!(change.already);
    assert!(change.rebuilt.is_empty(), "{:?}", change.rebuilt);
    assert_eq!(runs(&workspace), 1);
}

/// An undo that brings a tracker back rebuilds as one that drops it does,
/// and the rebuild is filled from the store: the instance built that
/// binding's tree when it spawned.
#[test]
fn undo_that_restores_a_wider_binding_fills_from_the_store() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager.spawn("x", None).expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    assert_eq!(members(&workspace), ["apps/a"], "precondition");

    let outcome = manager
        .undo("x", &undo_to(&checkpoint.record.id))
        .expect("undo");

    assert!(workspace.join("apps/b/index.js").exists(), "`b` is back");
    let action = deps_action(&outcome);
    assert!(
        action.contains("filled from install store"),
        "the tree for this binding is stored: {action}"
    );
    assert_eq!(members(&workspace), ["apps/a", "apps/b"]);
}

/// `undo --only` restores no binding, so the binding has not moved and the
/// skip applies as before.
#[test]
fn undo_only_keeps_the_binding_and_the_skip() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager
        .spawn_binding("x", None, &drop_b())
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    manager.use_tracker("x", "b").expect("use b");
    assert_eq!(members(&workspace), ["apps/a", "apps/b"], "precondition");

    let outcome = manager
        .undo(
            "x",
            &UndoOptions {
                only: vec!["deps".to_owned()],
                ..undo_to(&checkpoint.record.id)
            },
        )
        .expect("undo --only deps");

    assert!(
        workspace.join("apps/b/index.js").exists(),
        "`--only` leaves the binding"
    );
    assert!(
        deps_action(&outcome).contains("skipped"),
        "{}",
        deps_action(&outcome)
    );
    assert_eq!(members(&workspace), ["apps/a", "apps/b"]);
}
