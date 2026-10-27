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
use newgit_core::config::WorkspaceSection;
use newgit_core::installs::InstallReport;
use newgit_core::manager::{BindingChoice, BranchManager, Uncommitted, UndoOptions};
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
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("apps/a")).expect("mkdir");
    std::fs::create_dir_all(repo.join("apps/b")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("lock.txt"), "a@1\nb@1\n").expect("write");
    std::fs::write(repo.join("apps/a/index.js"), "a\n").expect("write");
    std::fs::write(repo.join("apps/b/index.js"), "b\n").expect("write");
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

    for (name, path) in [("a", "apps/a"), ("b", "apps/b")] {
        BranchManager::open(store.clone())
            .expect("manager")
            .create_code_tracker(name, "project-devs", &[Utf8PathBuf::from(path)])
            .expect("carve");
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    let manager = BranchManager::open(store).expect("manager");
    // The project root's own commits reach `main` like anyone's.
    manager
        .merge("main", "main")
        .expect("merge the project root");

    std::fs::write(
        manager.store().paths().resources.join("deps.toml"),
        r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]

[actions.prepare]
command = "mkdir -p node_modules && echo '{{bound.paths}}' > node_modules/members.txt"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#,
    )
    .expect("resource");
    BranchManager::open(manager.store().clone()).expect("reopen")
}

fn drop_b() -> BindingChoice {
    BindingChoice {
        drops: vec!["b".to_owned()],
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
