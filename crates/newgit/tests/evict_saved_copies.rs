//! A restore that fails or is cut short never deletes what is at the path:
//! it keeps it whole, beside the workspace, as a saved copy. Each copy is
//! recorded, said the moment it is made and by `start`, `status`,
//! `stop --evict`, and `cleanup` after, never taken for an orphan, kept by
//! `remove`, and deleted only by `newgit cleanup --discard-saved <path>`.

mod evict_support;

use camino::{Utf8Path, Utf8PathBuf};
use evict_support::*;

fn spawn(temp: &Utf8Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let store = setup(temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    std::fs::write(ws.join("untracked.txt"), "u\n").expect("write");
    (repo, ws)
}

/// Every place under `root` a file named `name` is.
fn found(root: &Utf8Path, name: &str) -> Vec<String> {
    listing(root)
        .into_iter()
        .filter(|path| path.rsplit('/').next() == Some(name))
        .collect()
}

fn read(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn evicted(repo: &Utf8Path) -> bool {
    newgit_ok(repo, &["status"]).stdout.contains("evicted")
}

const KILL: [(&str, &str); 1] = [("NEWGIT_TEST_KILL_AT", "restore-before-undo")];

/// The reviewer's scenario: kill a restore, write `a.txt`, kill the next
/// `start`, write `b.txt`, then `start` and `cleanup`. Both files survive,
/// in two saved copies, and every command names both.
#[test]
fn two_killed_restores_then_start_and_cleanup_keep_everything() {
    let (_guard, temp) = tempdir("evict-saved");
    let (repo, ws) = spawn(&temp);
    let before = snapshot(&ws);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let first_copy = temp.join("workspaces/.x.interrupted-restore");
    let second_copy = temp.join("workspaces/.x.interrupted-restore-2");

    let killed = newgit_env(&repo, &["start", "x"], &KILL);
    assert!(!killed.status.success());
    std::fs::write(ws.join("a.txt"), "a\n").expect("write");

    // The second killed `start` moves the first one's leftovers aside, and
    // says so before it is killed.
    let killed = newgit_env(&repo, &["start", "x"], &KILL);
    assert!(!killed.status.success());
    assert!(
        killed.stderr.contains(first_copy.as_str()),
        "said the moment it is made:\n{}",
        killed.stderr
    );
    std::fs::write(ws.join("b.txt"), "b\n").expect("write");

    let started = newgit_ok(&repo, &["start", "x"]);
    assert!(!evicted(&repo));
    // The copy this `start` made, the moment it made it; the earlier one
    // among the warnings.
    for copy in [&first_copy, &second_copy] {
        assert!(
            started.stderr.contains(copy.as_str()),
            "`start` names {copy}:\n{}",
            started.stderr
        );
    }
    assert!(started.stderr.contains("cleanup --discard-saved"));
    assert_same(&before, &snapshot(&ws));

    let status = newgit_ok(&repo, &["status"]);
    for copy in [&first_copy, &second_copy] {
        assert!(status.stdout.contains(copy.as_str()), "{}", status.stdout);
    }

    // Neither copy carries an instance marker, and `cleanup` keeps both.
    for copy in [&first_copy, &second_copy] {
        assert!(!copy.join(".newgit/local/instance.toml").exists());
    }
    let cleaned = newgit_ok(&repo, &["cleanup"]);
    assert!(
        !cleaned.stdout.contains("Orphan workspace"),
        "{}",
        cleaned.stdout
    );
    assert!(cleaned.stderr.contains(first_copy.as_str()));
    assert_eq!(read(&first_copy.join("a.txt")), "a\n");
    assert_eq!(read(&second_copy.join("b.txt")), "b\n");
    assert_eq!(found(&temp.join("workspaces"), "a.txt").len(), 1);
    assert_eq!(found(&temp.join("workspaces"), "b.txt").len(), 1);

    // `stop --evict` names them too.
    let evicting = newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(
        evicting.stderr.contains(second_copy.as_str()),
        "{}",
        evicting.stderr
    );

    // Discarded only when asked, by path, and only that one.
    newgit_ok(&repo, &["cleanup", "--discard-saved", first_copy.as_str()]);
    assert!(!first_copy.exists());
    assert!(second_copy.join("b.txt").exists());
    let status = newgit_ok(&repo, &["status"]);
    assert!(!status.stdout.contains(&format!("{first_copy} holds")));
    assert!(status.stdout.contains(&format!("{second_copy} holds")));

    // Anything else is refused.
    refused(
        &repo,
        &["cleanup", "--discard-saved", ws.as_str()],
        "not a saved copy",
    );

    // `remove` keeps what is left and names it; the copy is still
    // discardable by path once its record is archived.
    let removed = newgit_ok(&repo, &["remove", "x"]);
    assert!(
        removed.stdout.contains(second_copy.as_str()),
        "{}",
        removed.stdout
    );
    assert_eq!(read(&second_copy.join("b.txt")), "b\n");
    let cleaned = newgit_ok(&repo, &["cleanup"]);
    assert!(cleaned.stderr.contains(second_copy.as_str()));
    assert!(second_copy.join("b.txt").exists());
    newgit_ok(&repo, &["cleanup", "--discard-saved", second_copy.as_str()]);
    assert!(!second_copy.exists());
}

/// A process outside newgit writes into the path while a restore runs, and
/// the restore then fails. What it wrote after the restore's undo is kept,
/// whole, in `.x.restore-failed`; what it wrote before the undo replaced
/// the tree is in the undo's own checkpoint, which the failure names. Both
/// come back; nothing is deleted outside a checkpoint.
#[test]
fn writes_during_a_failed_restore_are_kept() {
    let (_guard, temp) = tempdir("evict-saved");
    let (repo, ws) = spawn(&temp);
    let before = snapshot(&ws);
    newgit_ok(&repo, &["stop", "x", "--evict"]);

    let pause = temp.join("pause");
    std::fs::create_dir_all(&pause).expect("mkdir");
    let (watched, signals) = (ws.clone(), pause.clone());
    let writer = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut early = false;
        while start.elapsed().as_secs() < 60 {
            if !early && watched.join(".git").exists() {
                std::fs::write(watched.join("early.txt"), "before the undo\n").expect("write");
                early = true;
            }
            if signals.join("restore-after-git.paused").exists() {
                std::fs::write(watched.join("late.txt"), "typed by the user\n").expect("write");
                std::fs::write(signals.join("restore-after-git.go"), "").expect("go");
                return early;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    });
    let failed = newgit_env(
        &repo,
        &["start", "x"],
        &[
            ("NEWGIT_TEST_FAIL_AT", "restore-after-git"),
            ("NEWGIT_TEST_PAUSE_AT", "restore-after-git"),
            ("NEWGIT_TEST_PAUSE_DIR", pause.as_str()),
        ],
    );
    assert!(writer.join().expect("writer"), "the writer saw `.git`");
    assert!(!failed.status.success());
    let copy = temp.join("workspaces/.x.restore-failed");
    assert!(
        failed.stderr.contains(copy.as_str()),
        "said the moment it is made:\n{}",
        failed.stderr
    );
    assert_eq!(read(&copy.join("late.txt")), "typed by the user\n");
    assert!(!copy.join(".newgit/local/instance.toml").exists());
    let safety = failed
        .stderr
        .split("before the restore's undo replaced it is in checkpoint ")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .unwrap_or_else(|| panic!("names the undo's checkpoint:\n{}", failed.stderr))
        .trim()
        .to_owned();
    assert!(evicted(&repo));
    // The path is back to its marker, so the next `start` restores.
    assert!(!ws.join(".git").exists());
    let started = newgit_ok(&repo, &["start", "x"]);
    assert!(started.stderr.contains(copy.as_str()));
    assert_same(&before, &snapshot(&ws));
    assert_eq!(read(&copy.join("late.txt")), "typed by the user\n");

    newgit_ok(&repo, &["undo", "x", "--to", &safety]);
    assert_eq!(read(&ws.join("early.txt")), "before the undo\n");
}

/// A restore that fails before anything reached the path leaves no copy:
/// there is nothing at the path but the marker.
#[test]
fn a_restore_failing_before_the_path_keeps_no_copy() {
    let (_guard, temp) = tempdir("evict-saved");
    let (repo, ws) = spawn(&temp);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let failed = newgit_env(
        &repo,
        &["start", "x"],
        &[("NEWGIT_TEST_FAIL_AT", "restore-after-clone")],
    );
    assert!(!failed.status.success());
    assert!(!temp.join("workspaces/.x.restore-failed").exists());
    assert!(!ws.join(".git").exists());
    newgit_ok(&repo, &["start", "x"]);
    assert!(ws.join("untracked.txt").exists());
}

/// `remove` on an evicted instance whose path holds work written after a
/// failed restore refuses, naming it, and `--force` deletes it.
#[test]
fn remove_refuses_work_at_an_evicted_path() {
    let (_guard, temp) = tempdir("evict-saved");
    let (repo, ws) = spawn(&temp);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let failed = newgit_env(
        &repo,
        &["start", "x"],
        &[("NEWGIT_TEST_FAIL_AT", "restore-after-clone")],
    );
    assert!(!failed.status.success());
    std::fs::write(ws.join("user.txt"), "mine\n").expect("write");

    let refusal = refused(&repo, &["remove", "x"], "not removing `x`");
    assert!(refusal.stderr.contains("`user.txt`"), "{}", refusal.stderr);
    assert!(refusal.stderr.contains("--force"));
    assert_eq!(read(&ws.join("user.txt")), "mine\n");
    assert!(evicted(&repo));

    newgit_ok(&repo, &["remove", "x", "--force"]);
    assert!(!ws.exists());
}
