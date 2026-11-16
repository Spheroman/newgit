//! `compose::find_outside_git_unpublished` (#444): what an export would
//! newly send, judged against a target that has everything the walk
//! excludes. A merge bringing a file the target has already beside new work
//! is not refused; the commit that added a file is, while the target lacks
//! it; and a merge whose file differs from every parent's still is.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose;
use newgit_core::tracker::{Storage, TrackerDefinition};

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(["-c", "commit.gpgSign=false"])
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
    let dir = std::env::temp_dir().join(format!("newgit-unpublished-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonical")).expect("utf8");
    (TempDir(path.clone()), path)
}

fn definitions() -> Vec<TrackerDefinition> {
    let mut env = TrackerDefinition::new(
        "env",
        "user".to_owned(),
        Storage::Local,
        false,
        vec![Utf8PathBuf::from(".env")],
    )
    .expect("definition");
    env.git = false;
    vec![TrackerDefinition::root(), env]
}

fn commit(repo: &Utf8Path, path: &str, contents: &str, message: &str) -> String {
    std::fs::write(repo.join(path), contents).expect("write");
    git(repo, &["add", "-f", path]);
    git(repo, &["commit", "-q", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

#[test]
fn only_what_the_target_lacks_is_judged() {
    let (_guard, repo) = tempdir();
    git(&repo, &["init", "-q", "-b", "main"]);
    let all = definitions();
    // The very first commit adds `.env`: found while the target has nothing.
    let root = commit(&repo, ".env", "TOKEN=published\n", "root adds .env");
    let found = compose::find_outside_git_unpublished(&repo, &["HEAD"], &all, None).expect("scan");
    assert_eq!(
        found.map(|(commit, path, owner)| (commit, path, owner)),
        Some((root.clone(), ".env".to_owned(), "env".to_owned()))
    );

    // The target has it now. New work on a line of its own, merged with
    // it: the merge holds `.env` as the target's commit has it.
    git(&repo, &["rm", "-q", "--cached", ".env"]);
    std::fs::remove_file(repo.join(".env")).expect("rm");
    git(&repo, &["commit", "-q", "-m", "drop .env"]);
    let published = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "--orphan", "side"]);
    commit(&repo, "work.txt", "work\n", "work");
    git(
        &repo,
        &[
            "merge",
            "-q",
            "--no-edit",
            "--allow-unrelated-histories",
            &root,
        ],
    );
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(&repo, &["rev-list", "--count", "--merges", "HEAD"]),
        "1"
    );
    let new = [tip.as_str(), "--not", published.as_str()];
    assert_eq!(
        compose::find_outside_git_unpublished(&repo, &new, &all, None).expect("scan"),
        None,
        "nothing new adds `.env`"
    );
    // Against each parent alone, the merge adds it beside the new line.
    assert!(
        compose::find_outside_git_under(&repo, &new, &[], &all, None)
            .expect("scan")
            .is_some()
    );

    // A merge with a `.env` of its own, unlike either parent's: refused.
    git(&repo, &["checkout", "-q", "-b", "evil", &published]);
    git(&repo, &["merge", "-q", "--no-edit", "--no-commit", "side"]);
    std::fs::write(repo.join(".env"), "TOKEN=new\n").expect("write");
    git(&repo, &["add", "-f", ".env"]);
    git(&repo, &["commit", "-q", "-m", "evil merge"]);
    let evil = git(&repo, &["rev-parse", "HEAD"]);
    let new = [evil.as_str(), "--not", published.as_str()];
    let found = compose::find_outside_git_unpublished(&repo, &new, &all, None).expect("scan");
    assert_eq!(
        found.map(|(commit, path, _)| (commit, path)),
        Some((evil, ".env".to_owned()))
    );
}
