//! The install store keys a tree on the part of the binding that can reach
//! the install, not on the whole binding (#248).
//!
//! Since #153 the key mixed in what `{{bound.trackers}}` and
//! `{{bound.paths}}` render to for every install, and since #206 `tracker
//! use`/`drop` rebuilt every install whenever that moved. Binding a secret
//! tracker with `paths = []`, which brings a branch database and cannot
//! change what `npm ci` sees, therefore cost a full install, and so did
//! creating or renaming a tracker anywhere in the project.
//!
//! What can reach an install is which paths are checked out (the sparse
//! checkout a dropped code tracker makes, which an unfiltered `npm ci`
//! reads by finding workspace members on disk), and the tracker names, only
//! when something tells the install them through `{{bound…}}`.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::installs::InstallReport;
use newgit_core::manager::{
    BindingChoice, BranchManager, ResourceBindOutcome, Uncommitted, UndoOptions,
};
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

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

/// Builds the members it finds on disk, as an unfiltered `npm ci` in an npm
/// workspace does, and counts every real run in `runs.txt`.
const DISCOVERING_INSTALL: &str =
    "echo run >> runs.txt && mkdir -p node_modules && ls apps > node_modules/members.txt";

/// The same, but told the binding: it records the bound tracker names.
const TOLD_INSTALL: &str = "echo run >> runs.txt && mkdir -p node_modules && ls apps > \
                            node_modules/members.txt && echo '{{bound.trackers}}' > \
                            node_modules/trackers.txt";

/// A workspace-style monorepo: one root lockfile listing two members, each
/// carved into its own code tracker, a secret tracker `branch-db` that owns
/// no paths (it only brings a database), and a shared install keyed on the
/// lockfile.
fn setup(temp: &Utf8Path, prepare: &str) -> BranchManager {
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
    BranchManager::open(store.clone())
        .expect("manager")
        .create_tracker("branch-db", "project-devs", Storage::Local, false)
        .expect("secret tracker");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    let manager = BranchManager::open(store).expect("manager");
    manager
        .merge("main", "main")
        .expect("merge the project root");

    define_resource(&manager, "deps", &deps_toml(prepare))
}

fn deps_toml(prepare: &str) -> String {
    format!(
        r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]

[actions.prepare]
command = "{prepare}"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#
    )
}

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

fn drops(names: &[&str]) -> BindingChoice {
    BindingChoice {
        drops: names.iter().map(|name| (*name).to_owned()).collect(),
        ..Default::default()
    }
}

fn deps(resources: &[ResourceBindOutcome]) -> &ResourceBindOutcome {
    resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps reported")
}

fn stored_key(outcome: &ResourceBindOutcome) -> String {
    match &outcome.install {
        Some(InstallReport::Stored { key, .. }) => key.clone(),
        other => panic!("the first instance should store its tree: {other:?}"),
    }
}

fn filled_key(outcome: &ResourceBindOutcome) -> String {
    assert!(
        outcome.prepare.is_none(),
        "the install must not run again: {outcome:?}"
    );
    match &outcome.install {
        Some(InstallReport::Filled { key, .. }) => key.clone(),
        other => panic!("the instance should be filled from the store: {other:?}"),
    }
}

/// How many times `deps`'s `prepare` really ran in `workspace`.
fn runs(workspace: &Utf8Path) -> usize {
    std::fs::read_to_string(workspace.join("runs.txt"))
        .map(|runs| runs.lines().count())
        .unwrap_or(0)
}

/// The issue's case: switching a branch database on is `tracker use` of a
/// secret tracker that owns no paths. Nothing `npm ci` sees changes, so the
/// install is kept, and not reported as rebuilt.
#[test]
fn using_a_pathless_secret_tracker_keeps_the_install() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, DISCOVERING_INSTALL);
    let spawned = manager
        .spawn_binding("x", None, &drops(&["branch-db"]))
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    assert_eq!(runs(&workspace), 1, "precondition: installed once");

    let used = manager.use_tracker("x", "branch-db").expect("use");
    assert!(used.rebuilt.is_empty(), "{:?}", used.rebuilt);
    assert_eq!(runs(&workspace), 1, "the use did not reinstall");
}

/// The same for `drop`, of a secret tracker that owns a file the install
/// never reads: its files leave the workspace, but no code path does.
#[test]
fn dropping_a_secret_tracker_keeps_the_install() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, DISCOVERING_INSTALL);
    manager
        .create_tracker("env", "project-devs", Storage::Local, false)
        .expect("create");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    let manager = BranchManager::open(manager.store().clone()).expect("reopen");
    let spawned = manager.spawn("x", None).expect("spawn x");
    let workspace = spawned.branch.workspace_path;

    let dropped = manager
        .drop_tracker("x", "env", Uncommitted::Refuse)
        .expect("drop");
    assert!(dropped.rebuilt.is_empty(), "{:?}", dropped.rebuilt);
    let used = manager.use_tracker("x", "env").expect("use");
    assert!(used.rebuilt.is_empty(), "{:?}", used.rebuilt);
    assert_eq!(runs(&workspace), 1, "neither change reinstalled");
}

/// At spawn, the default binding and `--use branch-db` (here: dropping it)
/// are the same tree, so the second is filled from the first.
#[test]
fn instances_that_differ_only_in_a_secret_tracker_share_one_tree() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, DISCOVERING_INSTALL);
    let with = manager.spawn("with", None).expect("spawn with");
    let key = stored_key(deps(&with.resources));

    let without = manager
        .spawn_binding("without", None, &drops(&["branch-db"]))
        .expect("spawn without");
    assert_eq!(filled_key(deps(&without.resources)), key);
}

/// Adding a tracker to the project does not move the key of an instance
/// that binds it, when it hides nothing from the checkout: before, every
/// tracker created or renamed re-keyed every install.
#[test]
fn creating_a_tracker_does_not_move_the_key() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, DISCOVERING_INSTALL);
    let first = manager.spawn("first", None).expect("spawn first");
    let key = stored_key(deps(&first.resources));

    manager
        .create_tracker("env", "project-devs", Storage::Local, false)
        .expect("create");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    let manager = BranchManager::open(manager.store().clone()).expect("reopen");

    let second = manager.spawn("second", None).expect("spawn second");
    assert_eq!(filled_key(deps(&second.resources)), key);
}

/// Dropping a code tracker still moves the key, and still rebuilds: its
/// files leave the checkout, so an install that finds its members on disk
/// builds less, without ever naming `{{bound…}}`.
#[test]
fn dropping_a_code_tracker_still_rebuilds_an_install_that_reads_the_disk() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, DISCOVERING_INSTALL);
    let spawned = manager.spawn("x", None).expect("spawn x");
    let workspace = spawned.branch.workspace_path;

    let change = manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    let rebuilt: Vec<&str> = change.rebuilt.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(rebuilt, ["deps"]);
    assert_eq!(
        std::fs::read_to_string(workspace.join("node_modules/members.txt")).expect("tree"),
        "a\n"
    );
}

/// An install told the tracker names builds something that depends on
/// them, so a secret tracker moves its key after all.
#[test]
fn an_install_that_names_the_binding_is_still_keyed_on_it() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, TOLD_INSTALL);
    let spawned = manager
        .spawn_binding("x", None, &drops(&["branch-db"]))
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;

    let change = manager.use_tracker("x", "branch-db").expect("use");
    let rebuilt: Vec<&str> = change.rebuilt.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(rebuilt, ["deps"]);
    let trackers =
        std::fs::read_to_string(workspace.join("node_modules/trackers.txt")).expect("tree");
    assert!(trackers.contains("branch-db"), "{trackers}");
}

/// The names reach an install without its own definition mentioning them:
/// every command's environment carries every bound resource's exports, so
/// an export that renders `{{bound.trackers}}` tells `deps` too.
#[test]
fn an_export_naming_the_binding_keys_every_install_on_it() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, DISCOVERING_INSTALL);
    let manager = define_resource(
        &manager,
        "meta",
        r#"ownership = "workspace"

[exports]
BOUND_TRACKERS = "{{bound.trackers}}"
"#,
    );
    let with = manager.spawn("with", None).expect("spawn with");
    let key = stored_key(deps(&with.resources));

    let without = manager
        .spawn_binding("without", None, &drops(&["branch-db"]))
        .expect("spawn without");
    match &deps(&without.resources).install {
        Some(InstallReport::Stored { key: other, .. }) => assert_ne!(other, &key),
        other => panic!("a different binding the install can read is a new tree: {other:?}"),
    }
}

/// Undo's `recompute` skip compares what the store key does: an undo that
/// rebinds only a pathless secret tracker leaves the install as it is.
#[test]
fn undo_across_a_secret_tracker_binding_skips_the_recompute() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, DISCOVERING_INSTALL);
    let spawned = manager
        .spawn_binding("x", None, &drops(&["branch-db"]))
        .expect("spawn x");
    let workspace = spawned.branch.workspace_path;
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    manager.use_tracker("x", "branch-db").expect("use");

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
    assert_eq!(runs(&workspace), 1, "the install ran once, at spawn");
}
