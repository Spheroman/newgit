//! A checkpoint's snapshot of uncommitted work is built in a throwaway index.
//! Starting it from `git read-tree HEAD` left it no stat data, so `git add -A`
//! hashed every tracked file: 3s a checkpoint once a project held 29,000. It
//! now starts from a copy of the real index, and what that copy may carry
//! over is what these tests hold down: the snapshot is the same tree the
//! empty index gave, whatever state the real index is in.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::source::GitSource;

fn run(dir: &Utf8Path, args: &[&str], index: Option<&str>) -> (bool, String, String) {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "ana")
        .env("GIT_AUTHOR_EMAIL", "ana@example.com")
        .env("GIT_COMMITTER_NAME", "ana")
        .env("GIT_COMMITTER_EMAIL", "ana@example.com");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().expect("git runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned(),
        String::from_utf8_lossy(&output.stderr)
            .trim_end()
            .to_owned(),
    )
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let (ok, stdout, stderr) = run(dir, args, None);
    assert!(ok, "git {args:?} failed: {stderr}");
    stdout
}

fn repo() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    git(&dir, &["init", "-q", "-b", "main"]);
    for name in ["a", "b", "c", "d", "e", "f"] {
        std::fs::write(dir.join(name), format!("{name}\n")).expect("write");
    }
    std::fs::create_dir(dir.join("sub")).expect("mkdir");
    std::fs::write(dir.join("sub/x"), "x\n").expect("write");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    (temp, dir)
}

/// The tree an index that starts empty gives: what a snapshot has always been.
fn from_empty_index(dir: &Utf8Path) -> String {
    let scratch = tempfile::tempdir().expect("tempdir");
    let index = scratch.path().join("index");
    let index = index.to_str();
    for args in [&["read-tree", "HEAD"][..], &["add", "-A", "--", "."][..]] {
        let (ok, _, stderr) = run(dir, args, index);
        assert!(ok, "git {args:?} failed: {stderr}");
    }
    run(dir, &["write-tree"], index).1
}

fn snapshot(dir: &Utf8Path) -> Option<String> {
    GitSource::workspace_dirty_commit(dir, "snapshot", &[], &[], &[]).expect("snapshot")
}

fn tree_of(dir: &Utf8Path, commit: &str) -> String {
    git(dir, &["rev-parse", &format!("{commit}^{{tree}}")])
}

fn assert_same_as_empty_index(dir: &Utf8Path) {
    let commit = snapshot(dir).expect("something changed");
    assert_eq!(tree_of(dir, &commit), from_empty_index(dir));
}

#[test]
fn staged_unstaged_deleted_renamed_and_untracked_work_is_all_in_the_snapshot() {
    let (_temp, dir) = repo();
    std::fs::write(dir.join("a"), "staged\n").expect("write");
    git(&dir, &["add", "a"]);
    std::fs::write(dir.join("a"), "staged, then edited\n").expect("write");
    std::fs::write(dir.join("b"), "unstaged\n").expect("write");
    std::fs::remove_file(dir.join("c")).expect("remove");
    git(&dir, &["rm", "-q", "--cached", "d"]);
    std::fs::rename(dir.join("e"), dir.join("e-moved")).expect("rename");
    std::fs::write(dir.join("new"), "new\n").expect("write");
    git(&dir, &["add", "new"]);
    std::fs::write(dir.join("untracked"), "untracked\n").expect("write");
    assert_same_as_empty_index(&dir);
}

#[test]
fn an_intent_to_add_entry_does_not_stop_the_snapshot() {
    let (_temp, dir) = repo();
    std::fs::write(dir.join("later"), "later\n").expect("write");
    git(&dir, &["add", "-N", "later"]);
    let commit = snapshot(&dir).expect("something changed");
    assert_eq!(git(&dir, &["show", &format!("{commit}:later")]), "later");
    assert_eq!(tree_of(&dir, &commit), from_empty_index(&dir));
}

#[test]
fn a_merge_conflict_does_not_stop_the_snapshot() {
    let (_temp, dir) = repo();
    git(&dir, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(dir.join("a"), "theirs\n").expect("write");
    git(&dir, &["commit", "-q", "-am", "theirs"]);
    git(&dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join("a"), "ours\n").expect("write");
    git(&dir, &["commit", "-q", "-am", "ours"]);
    let (merged, _, _) = run(&dir, &["merge", "theirs"], None);
    assert!(!merged, "the merge conflicts");
    assert_same_as_empty_index(&dir);
}

#[test]
fn an_edit_behind_a_skip_worktree_bit_is_in_the_snapshot() {
    let (_temp, dir) = repo();
    git(&dir, &["update-index", "--skip-worktree", "a"]);
    std::fs::write(dir.join("a"), "edited behind the bit\n").expect("write");
    let commit = snapshot(&dir).expect("something changed");
    assert_eq!(
        git(&dir, &["show", &format!("{commit}:a")]),
        "edited behind the bit"
    );
}

#[test]
fn an_edit_behind_both_bits_is_in_the_snapshot() {
    let (_temp, dir) = repo();
    git(&dir, &["update-index", "--skip-worktree", "a"]);
    git(&dir, &["update-index", "--assume-unchanged", "a"]);
    std::fs::write(dir.join("a"), "edited behind both\n").expect("write");
    let commit = snapshot(&dir).expect("something changed");
    assert_eq!(
        git(&dir, &["show", &format!("{commit}:a")]),
        "edited behind both"
    );
}

#[test]
fn an_edit_behind_an_assume_unchanged_bit_is_in_the_snapshot() {
    let (_temp, dir) = repo();
    git(&dir, &["update-index", "--assume-unchanged", "a"]);
    std::fs::write(dir.join("a"), "edited behind the bit\n").expect("write");
    let commit = snapshot(&dir).expect("something changed");
    assert_eq!(
        git(&dir, &["show", &format!("{commit}:a")]),
        "edited behind the bit"
    );
}

/// The race Git's own age check exists for: the file matches its index entry
/// in every way a stat can show (same inode, size, and mtime; ctime is not
/// trusted here) but its content moved on in the moment the index was
/// written. Only the entry being as new as the index file tells Git to look,
/// so a copy of the index dated now would vouch for it.
#[test]
fn an_edit_stat_cannot_see_in_the_instant_the_index_was_written_is_in_the_snapshot() {
    let (_temp, dir) = repo();
    git(&dir, &["config", "core.trustctime", "false"]);
    std::fs::write(dir.join("a"), "1\n").expect("write");
    let written = std::fs::metadata(dir.join("a"))
        .expect("stat")
        .modified()
        .expect("mtime");
    // Committed, so its entry is HEAD's and keeps its stat data.
    git(&dir, &["commit", "-q", "-am", "one"]);
    std::fs::write(dir.join("a"), "2\n").expect("write");
    for path in [dir.join("a"), dir.join(".git/index")] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(written)
            .expect("set mtime");
    }
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let commit = snapshot(&dir).expect("something changed");
    assert_eq!(git(&dir, &["show", &format!("{commit}:a")]), "2");
}

#[test]
fn a_clean_workspace_has_nothing_to_snapshot() {
    let (_temp, dir) = repo();
    assert_eq!(snapshot(&dir), None);
}

/// With no real index to copy it still works, only slower.
#[test]
fn a_workspace_without_an_index_still_snapshots() {
    let (_temp, dir) = repo();
    std::fs::remove_file(dir.join(".git/index")).expect("remove");
    std::fs::write(dir.join("a"), "edited\n").expect("write");
    let commit = snapshot(&dir).expect("something changed");
    assert_eq!(git(&dir, &["show", &format!("{commit}:a")]), "edited");
}

/// The point of it: a file whose stat data matches is not read at all. It
/// could not be, here, so the snapshot only succeeds if it was not.
#[test]
fn an_unchanged_file_is_not_read_again() {
    let (_temp, dir) = repo();
    // Past the second the index was written, so `a` is no longer racy.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    git(&dir, &["update-index", "--refresh"]);
    // `chmod` below moves `a`'s ctime, which would rightly make Git look.
    git(&dir, &["config", "core.trustctime", "false"]);
    std::fs::write(dir.join("b"), "edited\n").expect("write");
    let path = dir.join("a");
    let mut mode = std::fs::metadata(&path).expect("stat").permissions();
    let readable = mode.mode();
    mode.set_mode(0o000);
    std::fs::set_permissions(&path, mode.clone()).expect("chmod");
    let snapshotted = GitSource::workspace_dirty_commit(&dir, "snapshot", &[], &[], &[]);
    mode.set_mode(readable);
    std::fs::set_permissions(&path, mode).expect("chmod back");
    let commit = snapshotted.expect("a was not read").expect("b changed");
    assert_eq!(git(&dir, &["show", &format!("{commit}:b")]), "edited");
}
