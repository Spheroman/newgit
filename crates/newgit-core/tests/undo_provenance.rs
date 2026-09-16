//! #58: a pre-undo checkpoint's message describes when it was taken, and a
//! reader takes that as a description of what is in it. Those come apart
//! when the state it is about to capture is rubble a failed undo left
//! behind — this file covers the provenance note on the message, that
//! `newgit checkpoints` already gives a successful undo's safety checkpoint
//! its own reason distinct from both `explicit` and `failed-undo`, and that
//! a resource known to be broken does not get an expensive checkpoint
//! command run against it.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::checkpoint::CheckpointReason;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, UndoOptions};
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

fn read(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read {path}"))
}

/// Always fails to restore, and dumps to a counter file on every checkpoint
/// so tests can tell whether the (would-be expensive) capture command ran.
/// The counter lives outside the workspace: undo restores workspace source
/// to the checkpointed tree, which would silently wipe an untracked counter
/// file kept inside it (it was never part of what that checkpoint's source
/// snapshot captured), and that has nothing to do with what this resource's
/// *own* checkpoint command did or did not run.
fn dumps_on_checkpoint_never_restores(counter: &Utf8Path) -> String {
    format!(
        r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "echo dumped >> {counter}"

[restore]
mode = "command"
command = "exit 3"
"#
    )
}

/// The sentence the issue's author says they needed: a pre-undo checkpoint
/// minted right after an undo that did not finish should say its contents
/// may be rubble, not just when it was taken.
#[test]
fn a_safety_checkpoint_after_an_incomplete_undo_says_its_contents_may_be_partial() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "bad",
        &dumps_on_checkpoint_never_restores(&temp.join("counter.txt")),
    );
    let m = manager(store);
    m.spawn("feature-a", None).expect("spawn");
    m.checkpoint("feature-a", Some("good state"))
        .expect("checkpoint");

    // First undo: its own safety checkpoint precedes a clean prior state, so
    // it gets no provenance note.
    let first = m
        .undo("feature-a", &UndoOptions::default())
        .expect("first undo");
    assert!(!first.is_complete(), "restore command always fails");
    assert_eq!(
        first.safety.message.as_deref(),
        Some("state before undo to ckpt_001"),
        "nothing preceded this undo, so its safety checkpoint carries no provenance note"
    );

    // Second undo: the instance's last operation was that incomplete undo,
    // so the workspace this safety checkpoint is about to capture is
    // whatever the failed restore left behind.
    let second = m
        .undo("feature-a", &UndoOptions::default())
        .expect("second undo");
    assert!(!second.is_complete(), "restore command still always fails");
    let message = second.safety.message.expect("message");
    assert!(
        message.contains("captured after an incomplete undo; contents may be partial"),
        "message should carry provenance, got: {message}"
    );
    assert!(
        message.starts_with("state before undo to"),
        "the original message is preserved, not replaced: {message}"
    );
}

/// A successful undo's own safety checkpoint gets no provenance note, even
/// when a resource is still broken from before — the note is about the
/// *undo*, not about whether every resource is healthy.
#[test]
fn a_safety_checkpoint_after_a_clean_undo_carries_no_provenance_note() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = manager(store);
    m.spawn("feature-b", None).expect("spawn");
    m.checkpoint("feature-b", None).expect("checkpoint");

    let undo = m.undo("feature-b", &UndoOptions::default()).expect("undo");
    assert!(undo.is_complete());
    assert_eq!(
        undo.safety.message.as_deref(),
        Some("state before undo to ckpt_001")
    );
}

/// `newgit checkpoints`' REASON column (`checkpoints()` in the CLI) derives
/// its label from `(reason, undo_completed)`. A successful undo's safety
/// checkpoint (`BeforeUndo`, `Some(true)`) must land on a value distinct
/// from both an explicit checkpoint and a failed undo's — this is the fact
/// the CLI match arms encode; this test locks in the record shape they
/// switch on so the two cannot drift apart silently.
#[test]
fn a_completed_undos_safety_checkpoint_is_distinguishable_from_explicit_and_failed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = manager(store);
    m.spawn("feature-c", None).expect("spawn");
    m.checkpoint("feature-c", Some("named"))
        .expect("checkpoint");

    let undo = m.undo("feature-c", &UndoOptions::default()).expect("undo");
    assert!(undo.is_complete());

    let listed = m.list_checkpoints("feature-c").expect("list");
    let explicit = listed
        .iter()
        .find(|r| r.reason == CheckpointReason::Explicit)
        .expect("explicit checkpoint");
    let safety = listed
        .iter()
        .find(|r| r.id == undo.safety.id)
        .expect("safety checkpoint");

    // The pair the CLI matches on for each row.
    let explicit_key = (explicit.reason, explicit.undo_completed);
    let safety_key = (safety.reason, safety.undo_completed);
    assert_ne!(
        explicit_key, safety_key,
        "an explicit checkpoint and a completed undo's safety checkpoint must not collapse \
         to the same REASON"
    );
    assert_eq!(safety.reason, CheckpointReason::BeforeUndo);
    assert_eq!(
        safety.undo_completed,
        Some(true),
        "distinct from a failed undo's Some(false), which the REASON column already \
         renders as `failed-undo`"
    );
}

/// A resource whose last restore failed is known-broken, not merely
/// unobserved. Running its (real, possibly expensive) checkpoint command
/// again while minting the next safety checkpoint spends time dumping
/// rubble nobody is likely to ask to go back to, so it is skipped.
#[test]
fn a_pre_undo_checkpoint_skips_capturing_a_resource_broken_by_its_last_restore() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let runs_path = temp.join("counter.txt");
    write_resource(
        &store,
        "bad",
        &dumps_on_checkpoint_never_restores(&runs_path),
    );
    let m = manager(store);
    m.spawn("feature-d", None).expect("spawn");

    // Baseline explicit checkpoint: the resource is still healthy, so its
    // checkpoint command runs normally.
    m.checkpoint("feature-d", Some("good state"))
        .expect("checkpoint");
    assert_eq!(read(&runs_path).lines().count(), 1);

    // First undo: the resource is still `Ready` going in, so its safety
    // checkpoint captures it too — then the restore command fails, leaving
    // the resource `Failed`.
    let first = m
        .undo("feature-d", &UndoOptions::default())
        .expect("first undo");
    assert!(!first.is_complete());
    assert_eq!(read(&runs_path).lines().count(), 2);
    let bad_state = first
        .safety
        .resource_states
        .iter()
        .find(|s| s.name == "bad")
        .expect("bad in first safety checkpoint");
    assert_eq!(
        bad_state.mode, "command",
        "captured normally: still healthy"
    );

    // Second undo: the resource is now `Failed` from the first undo's
    // restore. Its checkpoint command must not run again.
    let second = m
        .undo("feature-d", &UndoOptions::default())
        .expect("second undo");
    assert_eq!(
        read(&runs_path).lines().count(),
        2,
        "the checkpoint command did not run a third time against known-broken state"
    );
    let bad_state = second
        .safety
        .resource_states
        .iter()
        .find(|s| s.name == "bad")
        .expect("bad in second safety checkpoint");
    assert_eq!(bad_state.mode, "none", "skipped, not captured");
    assert!(bad_state.state_ref.is_none());
    assert!(
        second
            .warnings
            .iter()
            .any(|w| w.contains("bad") && w.contains("failed state")),
        "warns about the skip: {:?}",
        second.warnings
    );
}

/// An explicit checkpoint still runs the command even against a resource
/// that is currently `Failed` — a person asked for this one on purpose, and
/// the skip is scoped to the automatic pre-undo path only.
#[test]
fn an_explicit_checkpoint_still_captures_a_failed_resource() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let runs_path = temp.join("counter.txt");
    write_resource(
        &store,
        "bad",
        &dumps_on_checkpoint_never_restores(&runs_path),
    );
    let m = manager(store);
    m.spawn("feature-e", None).expect("spawn");
    m.checkpoint("feature-e", None).expect("checkpoint");

    let undo = m.undo("feature-e", &UndoOptions::default()).expect("undo");
    assert!(!undo.is_complete(), "resource is now Failed");

    let runs_before = read(&runs_path).lines().count();
    let explicit = m
        .checkpoint("feature-e", Some("checking in anyway"))
        .expect("explicit checkpoint");
    assert_eq!(
        read(&runs_path).lines().count(),
        runs_before + 1,
        "an explicit checkpoint runs the command regardless of resource status"
    );
    let bad_state = explicit
        .record
        .resource_states
        .iter()
        .find(|s| s.name == "bad")
        .expect("bad state");
    assert_eq!(bad_state.mode, "command");
}

/// Guard against a typo/regression turning the never-branch into a panic:
/// undoing an instance with no prior checkpoint at all still works and
/// naturally carries no provenance note (there is nothing to be after).
#[test]
fn undo_with_no_prior_checkpoint_is_not_a_provenance_error() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = manager(store);
    m.spawn("feature-f", None).expect("spawn");
    let err = m
        .undo("feature-f", &UndoOptions::default())
        .expect_err("no checkpoint exists yet");
    assert!(matches!(err, NewgitError::NoCheckpoints(_)));
}
