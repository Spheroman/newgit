use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::{HookDetail, SnapshotRoots};
use newgit_core::manager::BranchManager;
use newgit_core::resource::Ownership;
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

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(newgit_core::config::WorkspaceSection {
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

fn manager_at(store: &MetadataStore) -> BranchManager {
    BranchManager::open(MetadataStore::at(store.paths().project_root.clone())).expect("manager")
}

#[test]
fn cleanup_hooks_respect_ownership_and_run_dependents_first() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    // `branch` ownership: newgit owns the instance, so the hook runs.
    write_resource(
        &store,
        "db",
        &format!(
            r#"kind = "command-snapshot"
ownership = "branch"
depends_on = ["store"]

[actions.prepare]
command = "true"

[cleanup]
command = "echo db >> {witness}/order.txt"
"#
        ),
    );
    // `user` ownership: shared beyond this project, so the hook must not run
    // even though one is defined.
    write_resource(
        &store,
        "store",
        &format!(
            r#"kind = "external-store"
ownership = "user"

[cleanup]
command = "echo store-WRONGLY-TORN-DOWN >> {witness}/order.txt"
"#
        ),
    );

    let manager = manager_at(&store);
    manager.spawn("feature-a", None).expect("spawn");
    let outcome = manager.remove("feature-a", &temp).expect("remove");

    let db = outcome
        .hooks
        .iter()
        .find(|hook| hook.resource == "db")
        .expect("db hook");
    assert!(matches!(db.detail, HookDetail::Ran { ok: true, .. }));

    let shared = outcome
        .hooks
        .iter()
        .find(|hook| hook.resource == "store")
        .expect("store hook");
    assert_eq!(shared.ownership, Ownership::User);
    assert_eq!(shared.detail, HookDetail::SkippedOwnership);

    // Dependents tear down before dependencies, and the user-owned store was
    // never touched.
    let order = std::fs::read_to_string(witness.join("order.txt")).expect("read order");
    assert_eq!(order, "db\n");
}

#[test]
fn a_cleanup_hook_with_an_unresolved_placeholder_is_refused() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    // The external template's shape: the teardown argument comes from a
    // checkpointed state ref. With no checkpoint, there is no argument —
    // and `delete {{state_ref}}` is not a safe thing to run anyway.
    write_resource(
        &store,
        "preview",
        &format!(
            r#"kind = "external"
ownership = "external"

[checkpoint]
mode = "external"
state_ref = "{{{{exports.PREVIEW_ID}}}}"

[cleanup]
command = "echo deleting {{{{state_ref}}}} >> {witness}/deleted.txt"
"#
        ),
    );

    let manager = manager_at(&store);
    manager.spawn("feature-a", None).expect("spawn");
    let outcome = manager.remove("feature-a", &temp).expect("remove");

    let hook = outcome
        .hooks
        .iter()
        .find(|hook| hook.resource == "preview")
        .expect("preview hook");
    match &hook.detail {
        HookDetail::SkippedUnresolved { placeholder, .. } => {
            assert_eq!(placeholder, "{{state_ref}}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(
        !witness.join("deleted.txt").exists(),
        "a command with a literal placeholder must never run"
    );
}

#[test]
fn a_checkpointed_state_ref_reaches_the_cleanup_hook() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    // prepare mints a handle, checkpoint records it, cleanup consumes it —
    // the whole external-resource lifecycle in one resource.
    write_resource(
        &store,
        "preview",
        &format!(
            r#"kind = "external"
ownership = "external"

[actions.prepare]
command = "echo '{{\"PREVIEW_ID\": \"pv_9\"}}'"
captures = ["PREVIEW_ID"]

[checkpoint]
mode = "external"
state_ref = "{{{{exports.PREVIEW_ID}}}}"

[cleanup]
command = "echo {{{{state_ref}}}} >> {witness}/deleted.txt"
"#
        ),
    );

    let manager = manager_at(&store);
    let spawned = manager.spawn("feature-a", None).expect("spawn");

    // The capture landed in the binding and is visible to commands.
    assert_eq!(
        spawned.branch.resources["preview"].resolved_exports["PREVIEW_ID"],
        "pv_9"
    );
    assert_eq!(spawned.resources[0].captured, ["PREVIEW_ID".to_owned()]);

    let checkpoint = manager.checkpoint("feature-a", None).expect("checkpoint");
    assert_eq!(
        checkpoint.record.resource_states[0].state_ref.as_deref(),
        Some("pv_9")
    );

    manager.remove("feature-a", &temp).expect("remove");
    assert_eq!(
        std::fs::read_to_string(witness.join("deleted.txt")).expect("read"),
        "pv_9\n"
    );
}

#[test]
fn cleanup_finalizes_workspaceless_instances_and_frees_the_name() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = manager_at(&store);
    manager.spawn("feature-a", None).expect("spawn a");
    let b = manager.spawn("feature-b", None).expect("spawn b");

    // Workspaces are disposable: someone deleted one directly.
    std::fs::remove_dir_all(&b.branch.workspace_path).expect("rm -rf workspace");

    let dry = manager.cleanup(true).expect("dry run");
    assert_eq!(dry.finalized.len(), 1);
    assert_eq!(dry.finalized[0].name, "feature-b");
    assert!(dry.finalized[0].archived_record.is_none());
    assert!(
        manager.store().find_branch("feature-b").is_ok(),
        "a dry run must not archive anything"
    );

    let outcome = manager.cleanup(false).expect("cleanup");
    assert_eq!(outcome.finalized.len(), 1);
    assert!(outcome.finalized[0].archived_record.is_some());
    assert!(manager.store().find_branch("feature-b").is_err());
    assert!(
        manager.store().find_branch("feature-a").is_ok(),
        "a healthy instance is left alone"
    );

    // Finalizing frees the name, which is the point: the instance was
    // unusable and unreplaceable before.
    assert!(manager.spawn("feature-b", None).is_ok());
}

#[test]
fn cleanup_deletes_unclaimed_workspaces_and_dead_state() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = manager_at(&store);
    let spawned = manager.spawn("feature-a", None).expect("spawn");

    // A workspace newgit made whose record was archived out from under it:
    // the marker proves newgit created it, so it is safe to delete.
    let marked = temp.join("workspaces/left-behind");
    let marker = newgit_core::materializer::workspace_marker_path(&marked);
    std::fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &marker,
        "branch = \"left-behind\"\nstore_root = \"/nowhere\"\n",
    )
    .expect("write marker");
    // An empty directory: nothing to lose either way.
    let empty = temp.join("workspaces/empty");
    std::fs::create_dir_all(&empty).expect("mkdir");
    // Someone else's directory. `[workspace] root` is user-configurable, so
    // newgit must not delete what it cannot prove it created.
    let foreign = temp.join("workspaces/my-notes");
    std::fs::create_dir_all(&foreign).expect("mkdir");
    std::fs::write(foreign.join("thesis.md"), "do not delete\n").expect("write");

    // A PID file whose process is long gone.
    let state_dir = store.instance_state_dir(&spawned.branch.slug);
    std::fs::create_dir_all(&state_dir).expect("mkdir");
    std::fs::write(state_dir.join("app.pid"), "999999\n").expect("write");
    // State for an instance that no longer exists at all.
    let ghost_state = store.instance_state_dir("ghost");
    std::fs::create_dir_all(&ghost_state).expect("mkdir");

    let outcome = manager.cleanup(false).expect("cleanup");

    assert_eq!(outcome.orphan_workspaces, [empty.clone(), marked.clone()]);
    assert!(!marked.exists(), "a marked workspace is newgit's to delete");
    assert!(!empty.exists());
    assert!(
        foreign.join("thesis.md").is_file(),
        "newgit must not delete a directory it cannot prove it created"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("my-notes")),
        "and it must say so rather than silently ignoring it: {:?}",
        outcome.warnings
    );

    assert!(!state_dir.join("app.pid").exists(), "dead PID file removed");
    assert!(!ghost_state.exists(), "state for a gone instance removed");
    assert!(
        spawned.branch.workspace_path.is_dir(),
        "a claimed workspace must survive"
    );
}

#[test]
fn pruning_never_drops_a_rev_a_checkpoint_still_points_at() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = manager_at(&store);
    manager
        .create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create tracker");
    manager
        .track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");

    let manager = manager_at(&store);
    let a = manager.spawn("feature-a", None).expect("spawn a");
    let b = manager.spawn("feature-b", None).expect("spawn b");

    // `a` captures, merges (so the lane head points at it), and checkpoints.
    std::fs::write(a.branch.workspace_path.join(".env.local"), "A=1\n").expect("write");
    let head_rev = manager
        .capture_tracker("feature-a", "runtime-env")
        .expect("capture a")
        .rev;
    manager
        .merge_tracker("feature-a", "runtime-env")
        .expect("merge");
    manager.checkpoint("feature-a", None).expect("checkpoint");

    // `a` then moves on, leaving the checkpointed rev referenced by nothing
    // but that checkpoint.
    std::fs::write(a.branch.workspace_path.join(".env.local"), "A=2\n").expect("write");
    let checkpointed = manager
        .capture_tracker("feature-a", "runtime-env")
        .expect("capture a2")
        .rev;
    manager.checkpoint("feature-a", None).expect("checkpoint 2");
    std::fs::write(a.branch.workspace_path.join(".env.local"), "A=3\n").expect("write");
    let bound = manager
        .capture_tracker("feature-a", "runtime-env")
        .expect("capture a3")
        .rev;

    // `b` captures content and is then thrown away without checkpointing:
    // its rev is the one true piece of garbage here.
    std::fs::write(b.branch.workspace_path.join(".env.local"), "B=1\n").expect("write");
    let orphaned = manager
        .capture_tracker("feature-b", "runtime-env")
        .expect("capture b")
        .rev;
    std::fs::remove_dir_all(&b.branch.workspace_path).expect("rm -rf");

    let all = [&head_rev, &checkpointed, &bound, &orphaned];
    assert_eq!(
        all.iter().collect::<std::collections::BTreeSet<_>>().len(),
        4,
        "the four captures must be distinct revs for this test to mean anything"
    );
    let lane = store.paths().snapshots.join("runtime-env");

    let dry = manager.cleanup(true).expect("dry run");
    let would_prune: Vec<&str> = dry.pruned.iter().map(|rev| rev.rev.as_str()).collect();
    assert_eq!(would_prune, [orphaned.as_str()]);
    assert!(lane.join(&orphaned).is_dir(), "a dry run removes nothing");

    let outcome = manager.cleanup(false).expect("cleanup");
    let pruned: Vec<&str> = outcome.pruned.iter().map(|rev| rev.rev.as_str()).collect();
    assert_eq!(pruned, [orphaned.as_str()]);
    assert!(!lane.join(&orphaned).exists(), "unreferenced rev pruned");
    assert!(
        lane.join(&checkpointed).is_dir(),
        "a rev only a checkpoint references must survive — pruning it breaks undo"
    );
    assert!(lane.join(&head_rev).is_dir(), "the lane head must survive");
    assert!(lane.join(&bound).is_dir(), "a bound rev must survive");
    assert!(outcome.pinned_by_checkpoints > 0);

    // Undo still works against the checkpoint whose content survived.
    let undone = manager
        .undo("feature-a", Some("ckpt_002"))
        .expect("undo to the checkpointed rev");
    assert_eq!(
        std::fs::read_to_string(a.branch.workspace_path.join(".env.local")).expect("read"),
        "A=2\n"
    );
    assert_eq!(
        undone.trackers[0].rev.as_deref(),
        Some(checkpointed.as_str())
    );
}

#[test]
fn snapshot_roots_separate_bindings_from_checkpoint_pins() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let manager = manager_at(&store);
    manager
        .create_tracker("env", "user", Storage::Local, false)
        .expect("create tracker");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");

    let manager = manager_at(&store);
    let a = manager.spawn("feature-a", None).expect("spawn");
    std::fs::write(a.branch.workspace_path.join(".env.local"), "A=1\n").expect("write");
    manager
        .capture_tracker("feature-a", "env")
        .expect("capture");
    manager.checkpoint("feature-a", None).expect("checkpoint");

    let branches = store.load_branches().expect("branches");
    let roots = SnapshotRoots::collect(&store, &branches).expect("roots");
    assert!(!roots.bindings.is_empty());
    assert!(!roots.checkpoints.is_empty());
    assert_eq!(
        roots.pinned_only_by_checkpoints().count(),
        0,
        "while the instance is still bound to it, the rev is not checkpoint-pinned"
    );

    // With no surviving instance, the same rev is held only by a checkpoint.
    let roots = SnapshotRoots::collect(&store, &[]).expect("roots");
    assert!(roots.bindings.is_empty());
    assert_eq!(roots.pinned_only_by_checkpoints().count(), 1);
}
