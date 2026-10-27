//! Two commands on one instance at once (#212). Nothing used to serialize
//! them: two checkpoints minted the same id and the second's record replaced
//! the first's; an `action` saved the whole record it had loaded before a
//! parallel one changed it; and a record was written in place, so a reader
//! could see it half-written. Now every command that changes an instance
//! holds that instance's lock from start to finish, and records are written
//! to a temporary file and renamed into place.
//!
//! Each manager here is opened in its own thread, so the lock is contended
//! for real: a manager hands its own nested calls the lock it holds, but
//! two managers are two holders.

use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::ResourceStatus;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::manager::{ActionOutcome, BranchManager};
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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

fn manager_at(root: &Utf8Path) -> BranchManager {
    BranchManager::open(MetadataStore::at(root.to_path_buf())).expect("manager")
}

fn wait_for(path: &Utf8Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{path} never appeared"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// `true` when nothing arrived on `rx` within `wait`: the sender is still
/// blocked on something.
fn still_pending<T>(rx: &mpsc::Receiver<T>, wait: Duration) -> bool {
    matches!(rx.recv_timeout(wait), Err(mpsc::RecvTimeoutError::Timeout))
}

/// The issue's resource: the first checkpoint command to run holds until a
/// marker appears, so a second checkpoint started meanwhile has to overlap
/// it — or wait for it.
#[test]
fn a_checkpoint_started_during_another_waits_and_gets_its_own_id() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let markers = temp.join("markers");
    std::fs::create_dir_all(&markers).expect("mkdir");
    let (lock, started, go) = (
        markers.join("lock"),
        markers.join("started"),
        markers.join("go"),
    );
    write_resource(
        &store,
        "db",
        &format!(
            r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "if mkdir {lock} 2>/dev/null; then touch {started}; while [ ! -f {go} ]; do sleep 0.05; done; echo first-state; else echo second-state; fi"
"#
        ),
    );
    manager_at(&root).spawn("x", None).expect("spawn");

    let first = {
        let root = root.clone();
        thread::spawn(move || manager_at(&root).checkpoint("x", Some("first")))
    };
    wait_for(&started);

    let (tx, rx) = mpsc::channel();
    let second = {
        let root = root.clone();
        thread::spawn(move || {
            let outcome = manager_at(&root).checkpoint("x", Some("second"));
            tx.send(()).expect("send");
            outcome
        })
    };
    assert!(
        still_pending(&rx, Duration::from_millis(500)),
        "the second checkpoint ran while the first was still capturing"
    );

    std::fs::write(&go, "").expect("go");
    let first = first.join().expect("thread").expect("first checkpoint");
    let second = second.join().expect("thread").expect("second checkpoint");
    assert_eq!(first.record.id, "ckpt_001");
    assert_eq!(second.record.id, "ckpt_002");

    let log = manager_at(&root).list_checkpoints("x").expect("list");
    let messages: Vec<Option<&str>> = log.iter().map(|r| r.message.as_deref()).collect();
    assert_eq!(messages, vec![Some("first"), Some("second")]);
    assert_eq!(
        log[0].resource_states[0].state_ref.as_deref(),
        Some("first-state")
    );
    assert_eq!(
        log[1].resource_states[0].state_ref.as_deref(),
        Some("second-state")
    );
}

/// `action` loads the record, runs the command, and saves the record back.
/// `deps.prepare` is held inside its command while `web.prepare` fails; when
/// `deps.prepare` finishes, the record it saves has to still say `web`
/// failed.
#[test]
fn an_action_finishing_after_a_parallel_one_does_not_overwrite_its_result() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let markers = temp.join("markers");
    std::fs::create_dir_all(&markers).expect("mkdir");
    let (hold, started, go, fail) = (
        markers.join("hold"),
        markers.join("started"),
        markers.join("go"),
        markers.join("fail"),
    );
    // Both succeed at spawn; the markers change what the explicit runs do.
    write_resource(
        &store,
        "deps",
        &format!(
            r#"ownership = "branch"

[actions.prepare]
command = "if [ -f {hold} ]; then touch {started}; while [ ! -f {go} ]; do sleep 0.05; done; fi"
"#
        ),
    );
    write_resource(
        &store,
        "web",
        &format!(
            r#"ownership = "branch"

[actions.prepare]
command = "if [ -f {fail} ]; then exit 1; fi"
"#
        ),
    );
    let spawned = manager_at(&root).spawn("x", None).expect("spawn");
    assert_eq!(
        spawned.branch.resources["web"].status,
        ResourceStatus::Ready
    );

    std::fs::write(&hold, "").expect("hold");
    std::fs::write(&fail, "").expect("fail");
    let deps = {
        let root = root.clone();
        thread::spawn(move || manager_at(&root).run_action("x", "deps.prepare"))
    };
    wait_for(&started);

    let (tx, rx) = mpsc::channel();
    let web = {
        let root = root.clone();
        thread::spawn(move || {
            let outcome = manager_at(&root).run_action("x", "web.prepare");
            tx.send(()).expect("send");
            outcome
        })
    };
    assert!(
        still_pending(&rx, Duration::from_millis(500)),
        "`web.prepare` ran while `deps.prepare` was still running"
    );

    std::fs::write(&go, "").expect("go");
    assert!(matches!(
        deps.join().expect("thread").expect("deps.prepare"),
        ActionOutcome::Ran { code: 0, .. }
    ));
    assert!(matches!(
        web.join().expect("thread").expect("web.prepare"),
        ActionOutcome::Ran { code: 1, .. }
    ));

    let branch = store.find_branch("x").expect("record");
    assert_eq!(branch.resources["deps"].status, ResourceStatus::Ready);
    assert_eq!(branch.resources["web"].status, ResourceStatus::Failed);
}

/// A record is renamed into place whole, so a reader never parses the
/// truncated middle of a write — `load_branches` reads every record, so one
/// instance being written used to break commands on every other.
#[test]
fn a_record_being_rewritten_is_never_read_half_written() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    manager_at(&root).spawn("x", None).expect("spawn");
    let mut record = store.find_branch("x").expect("record");

    let (done_tx, done_rx) = mpsc::channel();
    let writer = {
        let store = MetadataStore::at(root.clone());
        thread::spawn(move || {
            for _ in 0..2000 {
                record.updated_at = chrono::Utc::now();
                store.save_branch_record(&record).expect("save");
            }
            done_tx.send(()).expect("send");
        })
    };

    let mut loads = 0;
    while done_rx.try_recv().is_err() {
        let branches = store
            .load_branches()
            .unwrap_or_else(|error| panic!("load {loads} failed: {error}"));
        assert_eq!(branches.len(), 1);
        loads += 1;
    }
    writer.join().expect("writer");
    assert!(loads > 0);

    // Nothing but the record is left where records are listed from.
    let names: Vec<String> = std::fs::read_dir(&store.paths().branches)
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec!["x.toml"]);
}

/// A hook run under the lock is told which instance is locked, with the
/// token the lock file holds while it is, so a `newgit` it starts can refuse
/// rather than wait for its ancestor. A `newgit run` command holds no lock
/// and adds nothing to whatever it inherited.
#[test]
fn hooks_run_under_the_lock_are_told_so_and_run_commands_are_not() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let seen = temp.join("seen");
    let seen_lock = temp.join("seen-lock");
    let lock_file = store.locks_dir().join("x.lock");
    write_resource(
        &store,
        "db",
        &format!(
            r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "echo \"$NEWGIT_LOCKED\" > {seen} && cat {lock_file} > {seen_lock} && echo state"
"#
        ),
    );
    let manager = manager_at(&root);
    manager.spawn("x", None).expect("spawn");
    manager.checkpoint("x", None).expect("checkpoint");
    let token = std::fs::read_to_string(&seen_lock).expect("seen lock");
    assert!(
        token.starts_with(&format!("{}.", std::process::id())),
        "{token}"
    );
    let locked = std::fs::read_to_string(&seen).expect("seen");
    let entries: Vec<&str> = locked.trim().split(',').collect();
    assert_eq!(entries.last(), Some(&format!("x:{token}").as_str()));
    // Released: the token the hook saw matches nothing now.
    assert_eq!(std::fs::read_to_string(&lock_file).expect("lock"), "");

    manager
        .run_command(
            "x",
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                format!("echo \"$NEWGIT_LOCKED\" > {seen}"),
            ],
        )
        .expect("run");
    assert_eq!(
        std::fs::read_to_string(&seen).expect("seen").trim(),
        std::env::var("NEWGIT_LOCKED").unwrap_or_default()
    );
}

/// `cleanup` lists instances, then finishes the ones whose workspace is gone
/// one at a time under each one's lock. A `remove` holding that lock when
/// `cleanup` reaches it has torn the instance down by the time `cleanup`
/// gets it; `cleanup` used to carry on with the record it listed, rerun the
/// teardown, and fail the whole sweep archiving a record already archived.
#[test]
fn cleanup_skips_an_instance_removed_while_it_waited_for_it() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let spawned = manager_at(&root).spawn("x", None).expect("spawn");
    manager_at(&root).spawn("y", None).expect("spawn");
    std::fs::remove_dir_all(&spawned.branch.workspace_path).expect("rm workspace");

    // Stands in for a `remove x` in flight: it holds the lock, and archives
    // the record before letting go.
    let held = store.lock_instance("x", "x").expect("lock");
    let (tx, rx) = mpsc::channel();
    let cleanup = {
        let root = root.clone();
        thread::spawn(move || {
            let outcome = manager_at(&root).cleanup(false, ArchivedCheckpoints::Keep);
            tx.send(()).expect("send");
            outcome
        })
    };
    assert!(
        still_pending(&rx, Duration::from_millis(500)),
        "cleanup finished `x` while another command held its lock"
    );
    let record = store.find_branch("x").expect("record");
    store.archive_branch_record(&record).expect("archive");
    drop(held);

    let outcome = cleanup.join().expect("thread").expect("cleanup");
    assert!(outcome.finalized.is_empty(), "{:?}", outcome.finalized);
    assert!(store.find_branch("y").is_ok());
    // `x` has no record any more, so its lock file goes too; `y`'s stays.
    assert!(!store.locks_dir().join("x.lock").exists());
    assert!(store.locks_dir().join("y.lock").exists());
}

/// The manager's own nested calls — `checkpoint_verify` is a checkpoint, an
/// undo, and another checkpoint — share the lock the outer one holds.
#[test]
fn a_command_that_nests_others_in_process_does_not_wait_for_itself() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let manager = manager_at(&root);
    manager.spawn("x", None).expect("spawn");
    let verified = manager.checkpoint_verify("x", Some("v")).expect("verify");
    assert_eq!(verified.before.id, "ckpt_001");
}
