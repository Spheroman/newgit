//! `local_refs::sync` moves a ref without fetching only when the destination
//! holds everything the ref reaches; otherwise it fetches. A ref copied
//! without a fetch is complete, and one that could not be completed is never
//! written.
//!
//! "Without a fetch" is made observable by `protocol.file.allow = never` in
//! the destination: any `git fetch` from a local path fails there.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::local_refs;

fn git_output(dir: &Utf8Path, args: &[&str], input: Option<&[u8]>) -> Output {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("write");
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("git runs")
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = git_output(dir, args, None);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-local-refs-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

/// A work-tree repo with two commits on `main` and a tag on the first.
fn source(temp: &Utf8Path) -> Utf8PathBuf {
    let repo = temp.join("source");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a"), "one\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    git(&repo, &["tag", "v1"]);
    std::fs::write(repo.join("a"), "two\n").expect("write");
    git(&repo, &["commit", "-q", "-am", "two"]);
    git(&repo, &["update-ref", "refs/trackers/root/main", "HEAD"]);
    git(
        &repo,
        &["update-ref", "refs/trackers/root/instances/r1", "HEAD~1"],
    );
    repo
}

fn bare(temp: &Utf8Path, name: &str) -> Utf8PathBuf {
    let repo = temp.join(name);
    git(temp, &["init", "-q", "--bare", repo.as_str()]);
    repo
}

fn forbid_fetch(repo: &Utf8Path) {
    git(repo, &["config", "protocol.file.allow", "never"]);
}

fn resolves(repo: &Utf8Path, name: &str) -> Option<String> {
    let output = git_output(repo, &["rev-parse", "--verify", "--quiet", name], None);
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn complete(repo: &Utf8Path, name: &str) -> bool {
    git_output(repo, &["rev-list", "--objects", "--quiet", name], None)
        .status
        .success()
}

#[test]
fn a_destination_holding_the_history_gets_the_refs_without_a_fetch() {
    let (_guard, temp) = tempdir();
    let from = source(&temp);
    let to = bare(&temp, "to.git");
    // Everything `main` reaches, under another name: what the store and the
    // project repo share.
    local_refs::sync(&to, &from, &["+refs/heads/main:refs/held/main".to_owned()])
        .expect("first copy fetches");
    forbid_fetch(&to);

    local_refs::sync(
        &to,
        &from,
        &["+refs/trackers/root/*:refs/newgit/trackers/root/*".to_owned()],
    )
    .expect("a copy of what the destination holds needs no fetch");

    assert_eq!(
        resolves(&to, "refs/newgit/trackers/root/main"),
        resolves(&from, "refs/trackers/root/main")
    );
    assert_eq!(
        resolves(&to, "refs/newgit/trackers/root/instances/r1"),
        resolves(&from, "refs/trackers/root/instances/r1")
    );
    assert!(complete(&to, "refs/newgit/trackers/root/main"));
    // It copies what it names, and no tags.
    assert_eq!(resolves(&to, "refs/tags/v1"), None);
}

/// The destination has the tip commit object, but not its tree or parent:
/// "the object is here" is not "the history is here".
fn with_dangling_tip(temp: &Utf8Path, from: &Utf8Path) -> Utf8PathBuf {
    let to = bare(temp, "partial.git");
    let commit = git(from, &["cat-file", "commit", "main"]);
    let input = format!("{commit}\n");
    let output = git_output(
        &to,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--stdin",
            "--literally",
        ],
        Some(input.as_bytes()),
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        git(from, &["rev-parse", "main"])
    );
    to
}

#[test]
fn a_tip_present_without_its_history_is_fetched() {
    let (_guard, temp) = tempdir();
    let from = source(&temp);
    let to = with_dangling_tip(&temp, &from);

    local_refs::sync(
        &to,
        &from,
        &["+refs/heads/main:refs/copied/main".to_owned()],
    )
    .expect("sync");

    assert_eq!(resolves(&to, "refs/copied/main"), resolves(&from, "main"));
    assert!(
        complete(&to, "refs/copied/main"),
        "the ref was moved onto a history the destination does not hold"
    );
}

#[test]
fn a_ref_that_cannot_be_completed_is_never_written() {
    let (_guard, temp) = tempdir();
    let from = source(&temp);
    let to = with_dangling_tip(&temp, &from);
    forbid_fetch(&to);

    assert!(
        local_refs::sync(
            &to,
            &from,
            &["+refs/heads/main:refs/copied/main".to_owned()]
        )
        .is_err(),
        "with no fetch possible, the copy must fail rather than move the ref"
    );
    assert_eq!(resolves(&to, "refs/copied/main"), None);
}

#[test]
fn a_named_ref_the_source_lacks_fails_as_a_fetch_does() {
    let (_guard, temp) = tempdir();
    let from = source(&temp);
    let to = bare(&temp, "to.git");

    let error = local_refs::sync(
        &to,
        &from,
        &["+refs/heads/nope:refs/copied/nope".to_owned()],
    )
    .expect_err("no such ref");
    assert!(
        error.to_string().contains("couldn't find remote ref"),
        "{error}"
    );
}

#[test]
fn holds_complete_says_no_for_a_shallow_repo() {
    let (_guard, temp) = tempdir();
    let from = source(&temp);
    let shallow = temp.join("shallow");
    git(
        &temp,
        &[
            "clone",
            "-q",
            "--bare",
            "--depth=1",
            &format!("file://{from}"),
            shallow.as_str(),
        ],
    );
    let tip = git(&shallow, &["rev-parse", "HEAD"]);
    assert!(!local_refs::holds_complete(&shallow, &[&tip]));
    assert!(local_refs::holds_complete(
        &from.join(".git"),
        &[&git(&from, &["rev-parse", "HEAD"])]
    ));
}
