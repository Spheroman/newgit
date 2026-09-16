use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, UndoOptions};
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

fn manager(store: MetadataStore) -> BranchManager {
    BranchManager::open(store).expect("manager")
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

/// `[restore] mode = "recompute"`: the mechanism this issue is about. Never
/// restored on a fresh instance.
const RECOMPUTE_RESOURCE: &str = r#"ownership = "workspace"

[identity]
paths = ["README.md"]

[actions.prepare]
command = "echo run >> prep-runs.txt"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#;

/// `[restore] mode = "none"`: nothing to prove.
const NO_RESTORE_RESOURCE: &str = r#"ownership = "workspace"

[checkpoint]
mode = "none"

[restore]
mode = "none"
"#;

#[test]
fn a_never_restored_resource_is_named_unproven_on_the_checkpoint_line() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", RECOMPUTE_RESOURCE);
    let m = manager(store);
    m.spawn("feature-a", None).expect("spawn");

    let outcome = m.checkpoint("feature-a", None).expect("checkpoint");
    let state = &outcome.record.resource_states[0];
    assert!(
        state.restore_exercisable,
        "recompute is a real mechanism that can fail"
    );
    assert!(
        !state.restore_proven,
        "restore has never run on this instance yet"
    );

    // Exercising it for real — an ordinary undo that actually reruns the
    // recompute action, not one that skips because identity is unchanged —
    // is what proves it.
    m.undo(
        "feature-a",
        &UndoOptions {
            force_recompute: true,
            ..Default::default()
        },
    )
    .expect("undo");
    let outcome = m.checkpoint("feature-a", None).expect("checkpoint");
    let state = &outcome.record.resource_states[0];
    assert!(
        state.restore_proven,
        "restore just completed successfully on this instance"
    );
}

#[test]
fn a_recompute_skipped_for_unchanged_identity_does_not_count_as_proof() {
    // Identity never moves in this test, so every undo takes the "skipped:
    // identity unchanged" path — the command that could fail never runs,
    // so it must not be recorded as having proven anything.
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", RECOMPUTE_RESOURCE);
    let m = manager(store);
    m.spawn("feature-b", None).expect("spawn");

    m.checkpoint("feature-b", None).expect("checkpoint");
    let undo = m.undo("feature-b", &UndoOptions::default()).expect("undo");
    assert!(
        undo.resources[0].action.contains("skipped"),
        "expected the skip path, got {}",
        undo.resources[0].action
    );

    let outcome = m.checkpoint("feature-b", None).expect("checkpoint");
    let state = &outcome.record.resource_states[0];
    assert!(
        !state.restore_proven,
        "a skip never ran the command that could have failed"
    );
}

#[test]
fn a_resource_with_no_restore_command_needs_no_proof() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "static", NO_RESTORE_RESOURCE);
    let m = manager(store);
    m.spawn("feature-c", None).expect("spawn");

    let outcome = m.checkpoint("feature-c", None).expect("checkpoint");
    let state = &outcome.record.resource_states[0];
    assert!(
        !state.restore_exercisable,
        "`restore mode = \"none\"` runs nothing that could fail"
    );
}

/// `[checkpoint]`/`[restore]` command mode whose state ref is a pure
/// function of committed content — deterministic, so a verify run that
/// restores over it should always agree.
const DETERMINISTIC_COMMAND_RESOURCE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "cat README.md"

[restore]
mode = "command"
command = "true"
"#;

#[test]
fn verify_proves_a_restore_whose_state_ref_reproduces() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DETERMINISTIC_COMMAND_RESOURCE);
    let m = manager(store);
    m.spawn("feature-d", None).expect("spawn");

    let outcome = m
        .checkpoint_verify("feature-d", Some("proving db restore"))
        .expect("verify");

    assert!(outcome.undo.is_complete(), "the restore itself succeeded");
    assert_eq!(outcome.resources.len(), 1);
    let resource = &outcome.resources[0];
    assert!(resource.exercised);
    assert!(
        resource.agree,
        "before {:?} after {:?}",
        resource.before_state_ref, resource.after_state_ref
    );
    assert!(outcome.is_proven());

    // The proof is recorded on the binding, visible on the next checkpoint.
    let checked = m.checkpoint("feature-d", None).expect("checkpoint");
    assert!(checked.record.resource_states[0].restore_proven);
}

/// A checkpoint whose state ref is never the same twice — standing in for a
/// restore that runs (and exits 0) but leaves the resource in a different
/// state than the checkpoint recorded. This is the case `--verify` exists
/// to catch: `date` moves regardless of what `[restore]` did, the same way
/// a real resource's state can drift even though its restore "succeeded".
const DRIFTING_COMMAND_RESOURCE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "date +%s%N"

[restore]
mode = "command"
command = "true"
"#;

#[test]
fn verify_reports_a_mismatch_when_state_refs_disagree() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "flaky", DRIFTING_COMMAND_RESOURCE);
    let m = manager(store);
    m.spawn("feature-e", None).expect("spawn");

    let outcome = m
        .checkpoint_verify("feature-e", None)
        .expect("verify runs even when it finds a mismatch");

    let resource = &outcome.resources[0];
    assert!(resource.exercised);
    assert!(
        !resource.agree,
        "the clock moves between the two checkpoints, so before != after"
    );
    assert!(
        !outcome.is_proven(),
        "a state-ref mismatch must fail the verify even though the command exited 0"
    );

    // `restore_proven` and verify's `is_proven` answer different questions:
    // the restore command itself ran and exited 0 (that is what makes a
    // resource's `[restore]` "completed", the fact `restore_proven` is
    // about), but the state it landed on does not match what was
    // checkpointed. A mismatch is exactly the failure mode `--verify`
    // exists to catch that a plain "did it exit 0" check would miss.
    let checked = m.checkpoint("feature-e", None).expect("checkpoint");
    assert!(checked.record.resource_states[0].restore_proven);
}
