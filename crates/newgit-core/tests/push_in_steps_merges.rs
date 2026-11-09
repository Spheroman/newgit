//! A merge that brings more than the remote takes in one push. A fork that
//! keeps up with upstream by merging has them: Spheroman/t3code's "Merge
//! upstream main into newgit" brought 1,626 commits, 68.7 MiB, and stepping
//! along the first-parent line made it one step it could not split, so
//! copying the `t3code` tracker into an instance's fork failed after 23
//! minutes. A bare repository plays the remote, refusing a push of more than
//! ten new commits as Artifacts refuses one over 64 MiB.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::source::GitSource;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
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
    let dir = std::env::temp_dir().join(format!(
        "newgit-push-in-steps-merges-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
    (TempDir(path.clone()), path)
}

/// A bare remote that refuses a push bringing more than ten new commits,
/// in Artifacts' words.
fn limited_remote(temp: &Utf8Path) -> Utf8PathBuf {
    let remote = temp.join("remote.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", remote.as_str()],
    );
    let hook = remote.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         while read old new ref; do\n\
           [ \"$new\" = 0000000000000000000000000000000000000000 ] && continue\n\
           count=$(git rev-list \"$new\" --not --all | wc -l)\n\
           if [ \"$count\" -gt 10 ]; then\n\
             echo \"error: remote unpack failed: artifacts_git_receive_pack_request_too_large: push is 40.0 MiB, limit is 10.0 MiB\" >&2\n\
             exit 1\n\
           fi\n\
         done\n",
    )
    .expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    remote
}

fn commit(repo: &Utf8Path, name: &str) {
    std::fs::write(repo.join(format!("{name}.txt")), format!("{name}\n")).expect("write");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", name]);
}

/// Five commits on main, then a merge of `upstream`: thirty commits, one of
/// them a merge of twenty-five more. The merge is the tip, as it was.
fn fork_history(temp: &Utf8Path) -> (Utf8PathBuf, String) {
    let repo = temp.join("local");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for n in 0..5 {
        commit(&repo, &format!("main{n}"));
    }
    git(&repo, &["checkout", "-q", "-b", "upstream", "HEAD~3"]);
    for n in 0..15 {
        commit(&repo, &format!("up{n}"));
    }
    git(&repo, &["checkout", "-q", "-b", "feature"]);
    for n in 0..25 {
        commit(&repo, &format!("feature{n}"));
    }
    git(&repo, &["checkout", "-q", "upstream"]);
    git(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );
    for n in 15..30 {
        commit(&repo, &format!("up{n}"));
    }
    git(&repo, &["checkout", "-q", "main"]);
    git(
        &repo,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge upstream main",
            "upstream",
        ],
    );
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    (repo, tip)
}

fn assert_arrived(remote: &Utf8Path, repo: &Utf8Path, tip: &str) {
    assert_eq!(git(remote, &["rev-parse", "refs/heads/main"]), tip);
    assert_eq!(
        git(remote, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/main",
        "no staging ref is left"
    );
    assert_eq!(
        git(remote, &["rev-list", "--count", "main"]),
        git(repo, &["rev-list", "--count", "main"]),
        "every commit arrived"
    );
}

#[test]
fn a_leased_push_steps_through_a_merge_too_big_for_one_push() {
    let (_guard, temp) = tempdir();
    let remote = limited_remote(&temp);
    let (repo, tip) = fork_history(&temp);

    GitSource::push_leased(
        &["-C", repo.as_str()],
        remote.as_str(),
        &tip,
        "refs/heads/main",
        None,
        None,
    )
    .expect("pushed in steps");
    assert_arrived(&remote, &repo, &tip);
}

#[test]
fn a_copy_steps_through_a_merge_too_big_for_one_push() {
    let (_guard, temp) = tempdir();
    let remote = limited_remote(&temp);
    let (repo, tip) = fork_history(&temp);

    // As `copy_repo` pushes a fork: forced refspecs, no lease.
    GitSource::push_refs(
        &["-C", repo.as_str()],
        remote.as_str(),
        &["+refs/heads/main:refs/heads/main".to_owned()],
        None,
    )
    .expect("pushed in steps");
    assert_arrived(&remote, &repo, &tip);
}
