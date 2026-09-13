use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::branch::ResourceStatus;
use newgit_core::checkpoint::CheckpointReason;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, UndoOptions};
use newgit_core::store::MetadataStore;
use newgit_core::supervisor::Supervisor;
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

fn git_stdout(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join(".gitignore"), ".env.local\n").expect("write");
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

#[test]
fn checkpoint_and_undo_restore_source_trackers_and_dirty_state() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create tracker");
    m.track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    let m = manager(m.store().clone());

    let ws = m
        .spawn("feature-a", None)
        .expect("spawn")
        .branch
        .workspace_path;

    // Committed work, tracker content, and dirty state on top.
    std::fs::write(ws.join("src.txt"), "code\n").expect("write");
    git(&ws, &["add", "src.txt"]);
    git(&ws, &["commit", "-q", "-m", "add src"]);
    let good_head = git_stdout(&ws, &["rev-parse", "HEAD"]);
    std::fs::write(ws.join(".env.local"), "SECRET=1\n").expect("write");
    std::fs::write(ws.join("README.md"), "hello v2\n").expect("write");
    std::fs::write(ws.join("notes.txt"), "untracked notes\n").expect("write");

    let outcome = m
        .checkpoint("feature-a", Some("good state"))
        .expect("checkpoint");
    assert_eq!(outcome.record.source.head_rev, good_head);
    assert!(
        outcome.record.source.dirty_rev.is_some(),
        "worktree was dirty"
    );
    assert!(outcome.record.tracker_states[0].content_rev.is_some());
    // The store branch was blessed to the workspace head.
    let repo = m.store().paths().project_root.clone();
    assert_eq!(
        git_stdout(&repo, &["rev-parse", "refs/heads/feature-a"]),
        good_head
    );

    // The agent makes a mess: garbage commit, deleted file, mangled content.
    std::fs::write(ws.join("junk.txt"), "junk\n").expect("write");
    git(&ws, &["add", "junk.txt"]);
    git(&ws, &["commit", "-q", "-m", "junk"]);
    std::fs::remove_file(ws.join("notes.txt")).expect("rm");
    std::fs::write(ws.join("README.md"), "mangled\n").expect("write");
    std::fs::write(ws.join(".env.local"), "SECRET=evil\n").expect("write");

    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");
    assert_eq!(undo.restored.id, outcome.record.id);
    assert_eq!(undo.safety.reason, CheckpointReason::BeforeUndo);

    // Every layer is back: HEAD, dirty files, untracked file, tracker content.
    assert_eq!(git_stdout(&ws, &["rev-parse", "HEAD"]), good_head);
    assert_eq!(read(&ws.join("README.md")), "hello v2\n");
    assert_eq!(read(&ws.join("notes.txt")), "untracked notes\n");
    assert_eq!(read(&ws.join(".env.local")), "SECRET=1\n");
    assert!(!ws.join("junk.txt").exists(), "junk commit is gone");
    // The dirty state is uncommitted again, not baked into a commit.
    let status = git_stdout(&ws, &["status", "--porcelain"]);
    assert!(
        status.contains("README.md"),
        "README shows as modified: {status}"
    );
    // The workspace marker survived restore (it is self-ignored).
    assert!(ws.join(".newgit/local/instance.toml").is_file());

    // Undo twice is redo: the safety checkpoint brings the mess back.
    let redo = m.undo("feature-a", &UndoOptions::default()).expect("redo");
    assert_eq!(redo.restored.id, undo.safety.id);
    assert_eq!(read(&ws.join("README.md")), "mangled\n");
    assert_eq!(read(&ws.join(".env.local")), "SECRET=evil\n");
    assert!(ws.join("junk.txt").exists());
    assert!(!ws.join("notes.txt").exists());
}

#[test]
fn undo_to_targets_an_older_checkpoint() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = manager(store);
    let ws = m
        .spawn("feature-b", None)
        .expect("spawn")
        .branch
        .workspace_path;

    std::fs::write(ws.join("README.md"), "version one\n").expect("write");
    git(&ws, &["commit", "-qam", "v1"]);
    let first = m.checkpoint("feature-b", Some("v1")).expect("checkpoint");

    std::fs::write(ws.join("README.md"), "version two\n").expect("write");
    git(&ws, &["commit", "-qam", "v2"]);
    m.checkpoint("feature-b", Some("v2")).expect("checkpoint");

    m.undo(
        "feature-b",
        &UndoOptions {
            to: Some(first.record.id.clone()),
            ..Default::default()
        },
    )
    .expect("undo --to");
    assert_eq!(read(&ws.join("README.md")), "version one\n");

    assert!(matches!(
        m.undo(
            "feature-b",
            &UndoOptions {
                to: Some("ckpt_099".to_owned()),
                ..Default::default()
            }
        ),
        Err(NewgitError::UnknownCheckpoint { .. })
    ));
    // Explicit + explicit + before-undo safety.
    assert_eq!(m.list_checkpoints("feature-b").expect("list").len(), 3);
}

const HASH_RECOMPUTE_RESOURCE: &str = r#"ownership = "workspace"

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

#[test]
fn recompute_restore_skips_when_identity_is_unchanged() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", HASH_RECOMPUTE_RESOURCE);
    let m = manager(store);
    let ws = m
        .spawn("feature-c", None)
        .expect("spawn")
        .branch
        .workspace_path;
    assert_eq!(read(&ws.join("prep-runs.txt")).lines().count(), 1);

    let outcome = m.checkpoint("feature-c", None).expect("checkpoint");
    let state = &outcome.record.resource_states[0];
    assert_eq!(state.mode, "hash");
    assert!(
        state
            .state_ref
            .as_deref()
            .is_some_and(|r| r.starts_with("hash:"))
    );

    // `README.md` is untouched across the checkpoint, so the recorded hash
    // still describes the workspace and the rebuild is a no-op by
    // construction — which is the whole point of pointing `[identity]` at
    // the inputs.
    let undo = m.undo("feature-c", &UndoOptions::default()).expect("undo");
    assert_eq!(
        undo.resources[0].action,
        "recompute(prepare) skipped: identity unchanged"
    );
    assert!(undo.resources[0].ok);
    assert!(undo.recovery_record.is_none());
    assert_eq!(
        read(&ws.join("prep-runs.txt")).lines().count(),
        1,
        "prepare did not re-run"
    );

    // Move an identity path and the rebuild happens: the hash is what
    // decides, not the fact that a restore was requested.
    std::fs::write(ws.join("README.md"), "changed").expect("write");
    let undo = m.undo("feature-c", &UndoOptions::default()).expect("undo");
    assert_eq!(undo.resources[0].action, "recompute(prepare)");
    assert_eq!(read(&ws.join("prep-runs.txt")).lines().count(), 2);
}

/// The identity hash describes the inputs, not the tree: deleting half of
/// `node_modules` without touching the lockfile leaves the hash correct and
/// the tree wrong, so the repair path has to stay reachable.
#[test]
fn force_recompute_rebuilds_even_when_identity_is_unchanged() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", HASH_RECOMPUTE_RESOURCE);
    let m = manager(store);
    let ws = m
        .spawn("feature-h", None)
        .expect("spawn")
        .branch
        .workspace_path;
    m.checkpoint("feature-h", None).expect("checkpoint");

    let undo = m
        .undo(
            "feature-h",
            &UndoOptions {
                force_recompute: true,
                ..Default::default()
            },
        )
        .expect("undo");
    assert_eq!(undo.resources[0].action, "recompute(prepare)");
    assert_eq!(read(&ws.join("prep-runs.txt")).lines().count(), 2);
}

const DB_TRACKER_DEPOSIT_RESOURCE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "echo dump-data > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "cp {{state_ref}} restored.sql"
"#;

#[test]
fn into_tracker_deposits_into_lane_and_restore_reads_it_back() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB_TRACKER_DEPOSIT_RESOURCE);
    let m = manager(store);
    // Deposit-only lane: no owned workspace paths.
    m.create_tracker("db-snapshots", "project-devs", Storage::Local, false)
        .expect("create tracker");
    let m = manager(m.store().clone());
    let ws = m
        .spawn("feature-d", None)
        .expect("spawn")
        .branch
        .workspace_path;

    let outcome = m.checkpoint("feature-d", None).expect("checkpoint");
    let state = &outcome.record.resource_states[0];
    assert!(
        state
            .state_ref
            .as_deref()
            .is_some_and(|r| r.starts_with("tracker:db-snapshots@")),
        "state_ref was {:?}",
        state.state_ref
    );
    let state_path = state.state_path.clone().expect("state path");
    assert_eq!(read(&state_path), "dump-data\n");
    // The deposit is recorded as the deposit-only tracker's checkpoint rev.
    let tracker_state = outcome
        .record
        .tracker_states
        .iter()
        .find(|t| t.name == "db-snapshots")
        .expect("tracker state");
    assert!(tracker_state.content_rev.is_some());

    let undo = m.undo("feature-d", &UndoOptions::default()).expect("undo");
    assert!(undo.resources[0].ok);
    assert_eq!(read(&ws.join("restored.sql")), "dump-data\n");
}

const WORKDIR_CHECKPOINT_RESOURCE: &str = r#"ownership = "branch"
workdir = "packages/db"

[actions.prepare]
command = "true"

[checkpoint]
mode = "command"
command = "pwd > checkpoint-cwd.txt && pwd"

[restore]
mode = "command"
command = "pwd > restore-cwd.txt"
"#;

#[test]
fn checkpoint_and_restore_commands_honor_workdir() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::create_dir_all(repo.join("packages/db")).expect("mkdir");
    std::fs::write(repo.join("packages/db/.keep"), "").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "add packages/db"]);

    write_resource(&store, "db", WORKDIR_CHECKPOINT_RESOURCE);
    let m = manager(store);
    let ws = m
        .spawn("feature-a", None)
        .expect("spawn")
        .branch
        .workspace_path;

    m.checkpoint("feature-a", None).expect("checkpoint");
    assert_eq!(
        read(&ws.join("packages/db/checkpoint-cwd.txt")),
        format!("{}\n", ws.join("packages/db"))
    );

    m.undo("feature-a", &UndoOptions::default()).expect("undo");
    assert_eq!(
        read(&ws.join("packages/db/restore-cwd.txt")),
        format!("{}\n", ws.join("packages/db"))
    );
}

const BAD_RESTORE_RESOURCE: &str = r#"ownership = "branch"

[checkpoint]
mode = "none"

[restore]
mode = "command"
command = "exit 3"
"#;

#[test]
fn failed_restore_writes_recovery_record_and_restores_the_rest() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "bad", BAD_RESTORE_RESOURCE);
    write_resource(&store, "deps", HASH_RECOMPUTE_RESOURCE);
    let m = manager(store);
    let ws = m
        .spawn("feature-e", None)
        .expect("spawn")
        .branch
        .workspace_path;

    m.checkpoint("feature-e", None).expect("checkpoint");
    let undo = m.undo("feature-e", &UndoOptions::default()).expect("undo");

    let bad = undo
        .resources
        .iter()
        .find(|r| r.name == "bad")
        .expect("bad");
    assert!(!bad.ok);
    let deps = undo
        .resources
        .iter()
        .find(|r| r.name == "deps")
        .expect("deps");
    assert!(deps.ok, "other resources still restored");
    assert_eq!(
        read(&ws.join("prep-runs.txt")).lines().count(),
        1,
        "deps skipped: its identity did not move"
    );

    let recovery = undo.recovery_record.expect("recovery record");
    let contents = read(&recovery);
    assert!(
        contents.contains("bad"),
        "recovery names the resource: {contents}"
    );
    assert!(contents.contains("exited with 3"));

    let report = m
        .statuses()
        .expect("statuses")
        .into_iter()
        .find(|r| r.branch.name == "feature-e")
        .expect("report");
    let bad_state = &report.branch.resources.get("bad").expect("binding").status;
    assert_eq!(*bad_state, ResourceStatus::Failed);
}

const RUNNING_APP_RESOURCE: &str = r#"ownership = "branch"

[ports]
app = { start = 4210, env = "PORT" }

[actions.start]
command = "sleep 30"
long_running = true

[actions.stop]
signal = "term"
"#;

#[test]
fn undo_stops_running_processes_and_restarts_what_was_running() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", RUNNING_APP_RESOURCE);
    let m = manager(store);
    let spawn = m.spawn("feature-f", None).expect("spawn");
    let slug = spawn.branch.slug.clone();

    m.run_action("feature-f", "app.start").expect("start");
    let supervisor = Supervisor::new(m.store().instance_state_dir(&slug));
    let pid_before = supervisor.running_pid("app").expect("running");

    let outcome = m.checkpoint("feature-f", None).expect("checkpoint");
    assert!(outcome.record.resource_states[0].was_running);

    let undo = m.undo("feature-f", &UndoOptions::default()).expect("undo");
    assert!(undo.resources[0].action.ends_with("+ restarted"));
    let pid_after = supervisor.running_pid("app").expect("restarted");
    assert_ne!(pid_before, pid_after, "a fresh process was started");

    m.run_action("feature-f", "app.stop").expect("stop");
}

#[test]
fn identical_checkpoints_dedupe_lane_revs() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create tracker");
    m.track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    let m = manager(m.store().clone());
    let ws = m
        .spawn("feature-g", None)
        .expect("spawn")
        .branch
        .workspace_path;
    std::fs::write(ws.join(".env.local"), "SECRET=1\n").expect("write");

    let first = m.checkpoint("feature-g", None).expect("checkpoint");
    let second = m.checkpoint("feature-g", None).expect("checkpoint");
    assert_eq!(
        first.record.tracker_states[0].content_rev, second.record.tracker_states[0].content_rev,
        "identical content dedupes to the same lane rev"
    );
    assert_ne!(first.record.id, second.record.id);
}

/// A half-failed undo used to print `Restored` and `FAILED` about the same
/// operation, and left a pre-undo checkpoint indistinguishable from one a
/// human named — three failed attempts meant three of them, each pinning
/// tracker revs.
#[test]
fn an_incomplete_undo_says_so_and_marks_its_safety_checkpoint() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "bad", BAD_RESTORE_RESOURCE);
    write_resource(&store, "deps", HASH_RECOMPUTE_RESOURCE);
    let m = manager(store);
    m.spawn("feature-f", None).expect("spawn");
    m.checkpoint("feature-f", Some("good state"))
        .expect("checkpoint");

    let undo = m.undo("feature-f", &UndoOptions::default()).expect("undo");
    assert!(!undo.is_complete(), "one resource did not restore");
    assert_eq!(undo.failed_resources(), vec!["bad"]);
    assert_eq!(
        undo.safety.undo_completed,
        Some(false),
        "the safety checkpoint records that the undo it preceded failed"
    );

    // Recorded on disk, not just in the returned outcome, and visible in the
    // log so it can be told apart from a checkpoint a human chose.
    let listed = m.list_checkpoints("feature-f").expect("list");
    let safety = listed
        .iter()
        .find(|record| record.id == undo.safety.id)
        .expect("safety checkpoint listed");
    assert_eq!(safety.reason, CheckpointReason::BeforeUndo);
    assert_eq!(safety.undo_completed, Some(false));

    // The explicit checkpoint is never annotated — only pre-undo ones are.
    let explicit = listed
        .iter()
        .find(|record| record.reason == CheckpointReason::Explicit)
        .expect("explicit checkpoint");
    assert_eq!(explicit.undo_completed, None);
}

/// The complete case is the control: a clean undo is a redo point, and says so.
#[test]
fn a_complete_undo_marks_its_safety_checkpoint_as_a_redo_point() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", HASH_RECOMPUTE_RESOURCE);
    let m = manager(store);
    m.spawn("feature-g", None).expect("spawn");
    m.checkpoint("feature-g", None).expect("checkpoint");

    let undo = m.undo("feature-g", &UndoOptions::default()).expect("undo");
    assert!(undo.is_complete());
    assert!(undo.failed_resources().is_empty());
    assert_eq!(undo.safety.undo_completed, Some(true));
}
