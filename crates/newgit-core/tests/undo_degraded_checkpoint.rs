//! A checkpoint goes ahead without a resource that is not up and whose
//! capture fails, and `newgit checkpoint` says an undo to it "leaves them as
//! they are". The undo ran that resource's `[restore] command` anyway, with
//! `{{state_ref}}` unrendered, since the checkpoint recorded no state for it.
//! Every bundled database template drops or resets the database before it
//! reads the dump, so that undo destroyed the database it promised to leave
//! alone.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::Command;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::ResourceStatus;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{ActionOutcome, BranchManager, UndoOptions};
use newgit_core::store::MetadataStore;
use newgit_core::supervisor::Supervisor;

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

/// A database that never comes up and cannot be dumped, whose restore is
/// the bundled templates' shape minus the damage: it writes down what it
/// was handed instead of dropping anything.
fn unreachable_db(restored: &Utf8Path, extra: &str) -> String {
    format!(
        r#"ownership = "branch"

[actions.prepare]
command = "echo the database would not start; exit 1"
{extra}
[checkpoint]
mode = "command"
command = "exit 1"

[restore]
mode = "command"
command = "echo state_ref={{{{state_ref}}}} >> {restored}"
"#
    )
}

fn status_of(manager: &BranchManager, instance: &str, resource: &str) -> ResourceStatus {
    manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .resources[resource]
        .status
}

/// The resource's cell in `newgit status`.
fn reported_state(manager: &BranchManager, instance: &str, resource: &str) -> String {
    manager
        .statuses()
        .expect("statuses")
        .into_iter()
        .find(|report| report.branch.name == instance)
        .expect("instance report")
        .resources
        .into_iter()
        .find(|report| report.name == resource)
        .expect("resource report")
        .state
}

fn runs(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// What `path` holds once it has `lines` lines, waiting up to five seconds
/// for a process that has only just been started to write its own.
fn once_it_has(path: &Utf8Path, lines: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = runs(path);
        if text.lines().count() >= lines || Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn an_undo_to_a_degraded_checkpoint_leaves_the_resource_as_it_is() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let restored = temp.join("restored.txt");
    write_resource(&store, "db", &unreachable_db(&restored, ""));
    let m = manager(store);
    assert_eq!(
        m.spawn("feature-a", None)
            .expect("spawn")
            .failed_resources(),
        vec!["db"]
    );
    let checkpoint = m.checkpoint("feature-a", None).expect("checkpoint");
    assert_eq!(checkpoint.degraded, vec!["db"]);

    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");

    assert_eq!(
        runs(&restored),
        "",
        "a restore with nothing to restore from must never run"
    );
    assert_eq!(status_of(&m, "feature-a", "db"), ResourceStatus::Failed);
    assert_eq!(reported_state(&m, "feature-a", "db"), "failed");
    assert!(
        undo.warnings
            .iter()
            .any(|warning| warning.contains(&format!(
                "{} holds no state for resource `db`",
                checkpoint.record.id
            ))),
        "{:?}",
        undo.warnings
    );
    // Not restored, so not listed among what was: `spawn --from-checkpoint
    // --json` reports `action` and `ok` as null for it, as for any resource
    // the checkpoint holds nothing for. And not a failed undo: the
    // checkpoint said this is what an undo to it would do.
    assert!(
        undo.resources.iter().all(|resource| resource.name != "db"),
        "{:?}",
        undo.resources
    );
    assert!(undo.is_complete(), "{:?}", undo.resources);

    // The undo's own safety checkpoint skipped `db` too, as known-broken,
    // so a redo to it must leave `db` alone just the same.
    let redo = m.undo("feature-a", &UndoOptions::default()).expect("redo");
    assert_eq!(redo.restored.id, undo.safety.id);
    assert_eq!(runs(&restored), "", "the redo must not run it either");
    assert_eq!(status_of(&m, "feature-a", "db"), ResourceStatus::Failed);
    assert!(redo.is_complete(), "{:?}", redo.resources);
}

/// Left alone is not left running: nothing may run while the undo rewinds
/// content and re-renders config underneath it. Its process is stopped with
/// everyone else's and started again, as it was found, without its restore,
/// and the undo reports it like any resource it restarted.
#[test]
fn an_undo_restarts_what_a_left_alone_resource_was_running() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let restored = temp.join("restored.txt");
    let starts = temp.join("starts.txt");
    write_resource(
        &store,
        "db",
        &unreachable_db(
            &restored,
            &format!(
                r#"
[actions.start]
command = "echo started >> {starts}; sleep 30"
long_running = true

[actions.stop]
signal = "term"
"#
            ),
        ),
    );
    let m = manager(store);
    m.spawn("feature-a", None).expect("spawn");
    let ActionOutcome::Started { pid, .. } = m.run_action("feature-a", "db.start").expect("start")
    else {
        panic!("a long-running action starts");
    };
    let checkpoint = m.checkpoint("feature-a", None).expect("checkpoint");
    assert_eq!(checkpoint.degraded, vec!["db"]);

    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");

    let started = once_it_has(&starts, 2);
    let slug = m.store().find_branch("feature-a").expect("instance").slug;
    let running = Supervisor::new(m.store().instance_state_dir(&slug)).running_pid("db");
    m.run_action("feature-a", "db.stop").expect("stop");
    assert!(
        running.is_some_and(|running| running != pid),
        "stopped and started again: was {pid}, now {running:?}"
    );
    assert_eq!(started, "started\nstarted\n");
    assert_eq!(runs(&restored), "", "the restore still never runs");
    assert_eq!(status_of(&m, "feature-a", "db"), ResourceStatus::Failed);
    assert!(undo.is_complete(), "{:?}", undo.resources);
    let db = undo
        .resources
        .iter()
        .find(|resource| resource.name == "db")
        .expect("reported, since the undo stopped it");
    assert_eq!(db.action, "not restored + restarted");
    assert!(db.ok && !db.restart_failed, "{db:?}");
    assert_eq!(db.status, ResourceStatus::Failed, "as it was found: {db:?}");
}

/// A restart that fails is one like any other: the resource is marked failed
/// where it happens, and the undo is not complete. Here the database came up
/// after the checkpoint went ahead without it, so it is `Ready` going in.
#[test]
fn a_left_alone_resource_whose_restart_fails_is_marked_failed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let restored = temp.join("restored.txt");
    let up = temp.join("database-up");
    let db = |workdir: &str| {
        format!(
            r#"ownership = "branch"

[actions.prepare]
command = "test -f {up}"

[actions.start]
command = "sleep 30"
long_running = true
workdir = "{workdir}"

[actions.stop]
signal = "term"

[checkpoint]
mode = "command"
command = "test -f {up} && echo dump"

[restore]
mode = "command"
command = "echo state_ref={{{{state_ref}}}} >> {restored}"
"#
        )
    };
    write_resource(&store, "db", &db("."));
    let resources = store.paths().resources.clone();
    let m = manager(store);
    m.spawn("feature-a", None).expect("spawn");
    let checkpoint = m.checkpoint("feature-a", None).expect("checkpoint");
    assert_eq!(checkpoint.degraded, vec!["db"]);
    std::fs::write(&up, "").expect("up");
    m.run_action("feature-a", "db.prepare").expect("prepare");
    m.run_action("feature-a", "db.start").expect("start");
    assert_eq!(status_of(&m, "feature-a", "db"), ResourceStatus::Ready);

    // Point the start at a workdir that does not exist, so the restart the
    // undo attempts fails before anything runs.
    std::fs::write(resources.join("db.toml"), db("missing")).expect("rewrite");
    let m = BranchManager::open(m.store().clone()).expect("reopen");
    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");

    assert_eq!(runs(&restored), "", "the restore still never runs");
    let db = undo
        .resources
        .iter()
        .find(|resource| resource.name == "db")
        .expect("reported");
    assert!(db.ok && db.restart_failed, "{db:?}");
    assert_eq!(db.status, ResourceStatus::Failed, "{db:?}");
    assert!(!undo.is_complete());
    assert!(undo.recovery_record.is_some());
    assert_eq!(status_of(&m, "feature-a", "db"), ResourceStatus::Failed);
}

/// `checkpoint --verify` restores with a real undo, so it leaves the resource
/// alone too, and then has nothing to compare: neither checkpoint holds its
/// state. Two empty state refs agreeing is not a restore that was proven.
#[test]
fn verify_does_not_prove_a_restore_it_could_not_run() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let restored = temp.join("restored.txt");
    write_resource(&store, "db", &unreachable_db(&restored, ""));
    let m = manager(store);
    m.spawn("feature-a", None).expect("spawn");

    let outcome = m.checkpoint_verify("feature-a", None).expect("verify");

    assert_eq!(runs(&restored), "");
    assert!(outcome.undo.is_complete(), "{:?}", outcome.undo.resources);
    assert!(
        !outcome.is_proven(),
        "`db`'s restore never ran: {:?}",
        outcome.resources
    );
}

/// A checkpoint that did capture the resource but recorded no state ref for
/// it — a `command` checkpoint that printed nothing — leaves the same
/// placeholder unresolved. That restore is refused rather than handed a
/// literal `{{state_ref}}`, and the undo says it did not restore the
/// resource.
#[test]
fn a_restore_command_whose_state_ref_has_no_value_is_refused() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let restored = temp.join("restored.txt");
    write_resource(
        &store,
        "db",
        &format!(
            r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "true"

[restore]
mode = "command"
command = "echo state_ref={{{{state_ref}}}} >> {restored}"
"#
        ),
    );
    let m = manager(store);
    m.spawn("feature-a", None).expect("spawn");
    let checkpoint = m.checkpoint("feature-a", None).expect("checkpoint");
    assert!(checkpoint.degraded.is_empty());
    assert_eq!(checkpoint.record.resource_states[0].state_ref, None);

    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");

    assert_eq!(runs(&restored), "", "a literal placeholder must never run");
    assert_eq!(undo.failed_resources(), vec!["db"]);
    let recovery = undo
        .recovery_record
        .as_ref()
        .expect("a restore that did not happen is recorded as one");
    assert!(recovery.is_file(), "{recovery}");
    assert_eq!(status_of(&m, "feature-a", "db"), ResourceStatus::Failed);
}

/// Left alone is a resource whose dump or handle the checkpoint was meant to
/// hold and does not. A `hash` checkpoint records only the identity of the
/// inputs a `recompute` rebuilds from, so when the safety checkpoint skips
/// one, the redo still rebuilds — from the inputs it has just put back.
#[test]
fn a_recompute_behind_a_skipped_hash_still_rebuilds() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let allowed = temp.join("install-allowed");
    let builds = temp.join("builds.txt");
    write_resource(
        &store,
        "deps",
        &format!(
            r#"ownership = "workspace"

[identity]
paths = ["README.md"]

[actions.prepare]
command = "test -f {allowed} && echo built >> {builds}"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
"#
        ),
    );
    let m = manager(store);
    assert_eq!(
        m.spawn("feature-a", None)
            .expect("spawn")
            .failed_resources(),
        vec!["deps"]
    );
    m.checkpoint("feature-a", None).expect("checkpoint");
    std::fs::write(&allowed, "").expect("allow");
    // Its safety checkpoint skips `deps` as failed; the restore rebuilds it.
    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");
    assert!(undo.is_complete(), "{:?}", undo.resources);
    assert_eq!(runs(&builds), "built\n");

    let redo = m.undo("feature-a", &UndoOptions::default()).expect("redo");

    assert_eq!(redo.restored.id, undo.safety.id);
    let deps = redo
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("restored");
    assert_eq!(deps.action, "recompute(prepare)", "{:?}", redo.resources);
    assert_eq!(runs(&builds), "built\nbuilt\n");
}

/// The same holds for a `command` checkpoint paired with a `recompute`
/// restore, which the definition rules allow because a `recompute` ignores
/// the ref: the checkpoint goes ahead without the resource, and the undo
/// still rebuilds it from the inputs it puts back.
#[test]
fn a_recompute_behind_a_skipped_command_checkpoint_still_rebuilds() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let allowed = temp.join("install-allowed");
    let builds = temp.join("builds.txt");
    write_resource(
        &store,
        "deps",
        &format!(
            r#"ownership = "workspace"

[actions.prepare]
command = "test -f {allowed} && echo built >> {builds}"

[checkpoint]
mode = "command"
command = "test -f {allowed} && echo captured"

[restore]
mode = "recompute"
"#
        ),
    );
    let m = manager(store);
    assert_eq!(
        m.spawn("feature-a", None)
            .expect("spawn")
            .failed_resources(),
        vec!["deps"]
    );
    let checkpoint = m.checkpoint("feature-a", None).expect("checkpoint");
    assert_eq!(checkpoint.degraded, vec!["deps"]);
    std::fs::write(&allowed, "").expect("allow");

    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");

    let deps = undo
        .resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("restored");
    assert_eq!(deps.action, "recompute(prepare)", "{:?}", undo.resources);
    assert!(undo.is_complete(), "{:?}", undo.resources);
    assert_eq!(runs(&builds), "built\n");
    assert_eq!(status_of(&m, "feature-a", "deps"), ResourceStatus::Ready);
}
