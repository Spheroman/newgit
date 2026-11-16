//! An eviction or a restore that fails or is killed part way never deletes
//! work it cannot prove is its own; what would be lost or broken is
//! refused, deleting nothing (submodules, linked worktrees, a shell sitting
//! in the workspace); and judging thousands of ignored files is quick.

mod evict_support;

use camino::Utf8Path;
use evict_support::*;

fn spawn(temp: &Utf8Path) -> (camino::Utf8PathBuf, camino::Utf8PathBuf) {
    let store = setup(temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    std::fs::write(ws.join("untracked.txt"), "u\n").expect("write");
    chmod(&ws.join("untracked.txt"), 0o600);
    (repo, ws)
}

fn evicted(repo: &Utf8Path) -> bool {
    newgit_ok(repo, &["status"]).stdout.contains("evicted")
}

/// A late restore step fails: the path goes back to its marker, the
/// record still says evicted, and work written there afterwards is never
/// deleted: `stop --evict` and `start` both refuse, and the next `start`
/// after it is moved away gives back exactly what was evicted.
#[test]
fn a_failed_restore_then_work_then_start_deletes_nothing() {
    let (_guard, temp) = tempdir("evict-int");
    let (repo, ws) = spawn(&temp);
    let before = snapshot(&ws);
    newgit_ok(&repo, &["stop", "x", "--evict"]);

    let failed = newgit_env(
        &repo,
        &["start", "x"],
        &[("NEWGIT_TEST_FAIL_AT", "restore-after-git")],
    );
    assert!(!failed.status.success(), "{}", failed.stdout);
    assert!(evicted(&repo));
    assert!(
        !ws.join("untracked.txt").exists() && !ws.join(".git").exists(),
        "a failed restore leaves the marker, not half a workspace"
    );

    // A resumed agent writes there anyway.
    std::fs::write(ws.join("agent.txt"), "new work\n").expect("write");
    refused(
        &repo,
        &["stop", "x", "--evict"],
        "holds more than its marker",
    );
    refused(&repo, &["start", "x"], "holds more than its marker");
    assert_eq!(read(&ws.join("agent.txt")), "new work\n");

    std::fs::rename(ws.join("agent.txt"), temp.join("agent.txt")).expect("move");
    newgit_ok(&repo, &["start", "x"]);
    assert!(!evicted(&repo));
    assert_same(&before, &snapshot(&ws));
}

/// A restore killed part way leaves half a workspace that anyone may have
/// worked in since: `stop --evict` refuses, and the next `start` moves it
/// aside whole, says where, and restores.
#[test]
fn a_killed_restore_then_work_then_start_keeps_the_work_aside() {
    let (_guard, temp) = tempdir("evict-int");
    let (repo, ws) = spawn(&temp);
    let before = snapshot(&ws);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let killed = newgit_env(
        &repo,
        &["start", "x"],
        &[("NEWGIT_TEST_KILL_AT", "restore-before-undo")],
    );
    assert_eq!(killed.status.code(), Some(137), "{}", killed.stderr);
    assert!(ws.join(".git").exists(), "the kill left a half workspace");

    std::fs::write(ws.join("agent.txt"), "new work\n").expect("write");
    refused(
        &repo,
        &["stop", "x", "--evict"],
        "holds more than its marker",
    );
    let started = newgit_ok(&repo, &["start", "x"]);
    assert!(
        started.stderr.contains(".x.interrupted-restore"),
        "{}",
        started.stderr
    );
    assert_eq!(
        read(&temp.join("workspaces/.x.interrupted-restore/agent.txt")),
        "new work\n"
    );
    assert_same(&before, &snapshot(&ws));
}

/// An eviction killed after recording itself and before moving the
/// workspace leaves the original at the path: `start` recognises it,
/// clears `evicted`, and the workspace is as it was; `run` works again.
#[test]
fn an_eviction_killed_before_the_move_leaves_the_workspace_as_it_was() {
    let (_guard, temp) = tempdir("evict-int");
    let (repo, ws) = spawn(&temp);
    std::fs::create_dir_all(ws.join("target")).expect("mkdir");
    std::fs::write(ws.join("target/build.o"), "o\n").expect("write");
    let before = snapshot(&ws);
    let killed = newgit_env(
        &repo,
        &["stop", "x", "--evict", "--discard-ignored"],
        &[("NEWGIT_TEST_KILL_AT", "evict-after-record")],
    );
    assert_eq!(killed.status.code(), Some(137), "{}", killed.stderr);
    assert!(evicted(&repo));
    let started = newgit_ok(&repo, &["start", "x"]);
    assert!(!started.stdout.contains("Restored"), "{}", started.stdout);
    assert!(!evicted(&repo));
    let mut after = snapshot(&ws);
    // The eviction's checkpoint is the one change it leaves.
    after.insert("@refs".to_owned(), before["@refs"].clone());
    assert_same(&before, &after);
    assert_eq!(
        read(&ws.join("target/build.o")),
        "o\n",
        "never moved, so kept"
    );
    newgit_ok(&repo, &["run", "x", "--", "true"]);
}

/// An initialized submodule and a linked worktree are refused, naming
/// them, and nothing is deleted.
#[test]
fn submodules_and_linked_worktrees_are_refused() {
    let (_guard, temp) = tempdir("evict-int");
    let (repo, ws) = spawn(&temp);

    let sub = temp.join("sub");
    std::fs::create_dir_all(&sub).expect("mkdir");
    git(&sub, &["init", "-q", "-b", "main"]);
    std::fs::write(sub.join("f.txt"), "f\n").expect("write");
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-q", "-m", "sub"]);
    let sub_url = sub.to_string();
    git(
        &ws,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &sub_url,
            "vendor/sub",
        ],
    );
    git(&ws, &["commit", "-q", "-m", "add sub"]);
    std::fs::write(ws.join("vendor/sub/f.txt"), "local edit\n").expect("write");
    let before = snapshot(&ws);
    refused(
        &repo,
        &["stop", "x", "--evict"],
        "`vendor/sub` is an initialized submodule",
    );
    assert_same(&before, &snapshot(&ws));
    git(&ws, &["submodule", "deinit", "-q", "-f", "vendor/sub"]);

    let wt = temp.join("wt");
    let wt_str = wt.to_string();
    git(&ws, &["worktree", "add", "-q", &wt_str, "-b", "side"]);
    std::fs::write(wt.join("staged.txt"), "s\n").expect("write");
    git(&wt, &["add", "staged.txt"]);
    refused(
        &repo,
        &["stop", "x", "--evict"],
        "a worktree linked to this workspace",
    );
    assert!(ws.join(".git").exists());
    assert!(git_out(&wt, &["status", "--porcelain"]).contains("A  staged.txt"));
}

/// Run from a shell whose working directory is the workspace, `stop
/// --evict` refuses and says to run it from outside; run with only its
/// own working directory there (no shell), it evicts: its `lsof` is not
/// mistaken for a process in the workspace.
#[test]
fn a_shell_in_the_workspace_is_refused_and_newgit_itself_is_not() {
    let (_guard, temp) = tempdir("evict-int");
    let (repo, ws) = spawn(&temp);
    let before = snapshot(&ws);
    let output = std::process::Command::new("sh")
        .current_dir(ws.as_str())
        .args([
            "-c",
            &format!("'{}' stop x --evict; exit $?", env!("CARGO_BIN_EXE_newgit")),
        ])
        .output()
        .expect("sh runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("from outside the workspace"), "{stderr}");
    assert_same(&before, &snapshot(&ws));

    newgit_ok(&ws, &["stop", "x", "--evict"]);
    assert!(!ws.join("untracked.txt").exists());
    newgit_ok(&repo, &["start", "x"]);
    assert_same(&before, &snapshot(&ws));
}

/// Twenty thousand ignored build files are judged, summarised as their
/// directory with a count, and refused in well under a few seconds.
#[test]
fn many_ignored_files_are_judged_quickly() {
    let (_guard, temp) = tempdir("evict-int");
    let (repo, ws) = spawn(&temp);
    for dir in 0..100 {
        let path = ws.join(format!("target/debug/{dir}"));
        std::fs::create_dir_all(&path).expect("mkdir");
        for file in 0..200 {
            std::fs::write(path.join(format!("{file}.o")), "").expect("write");
        }
    }
    let started = std::time::Instant::now();
    let run = refused(&repo, &["stop", "x", "--evict"], "`target/ (20000 files)`");
    let took = started.elapsed();
    eprintln!("judged 20000 ignored files in {took:?}");
    assert!(took.as_secs_f64() < 3.0, "took {took:?}: {}", run.stderr);
}

fn read(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read {path}"))
}
