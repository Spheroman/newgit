//! A push too big for the remote goes in steps. Artifacts refuses any one push
//! over 64 MiB (`artifacts_git_receive_pack_request_too_large: push is
//! 364.7 MiB, limit is 64.0 MiB`), which an imported history passes easily:
//! Spheroman/t3code is 365 MiB. A bare repository here plays the remote,
//! with a hook that refuses a push of more than ten new commits the same way.
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
    let dir = std::env::temp_dir().join(format!("newgit-push-in-steps-{}-{n}", std::process::id()));
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

/// Forty commits, one file each, with a merge partway along.
fn history(temp: &Utf8Path) -> (Utf8PathBuf, String) {
    let repo = temp.join("local");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for n in 0..40 {
        if n == 20 {
            git(&repo, &["checkout", "-q", "-b", "side"]);
            for k in 0..5 {
                std::fs::write(repo.join(format!("side{k}.txt")), "side\n").expect("write");
                git(&repo, &["add", "-A"]);
                git(&repo, &["commit", "-q", "-m", &format!("side {k}")]);
            }
            git(&repo, &["checkout", "-q", "main"]);
            git(
                &repo,
                &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
            );
        }
        std::fs::write(repo.join(format!("file{n}.txt")), format!("{n}\n")).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("commit {n}")]);
    }
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    (repo, tip)
}

#[test]
fn a_push_too_big_for_the_remote_goes_in_steps() {
    let (_guard, temp) = tempdir();
    let remote = limited_remote(&temp);
    let (repo, tip) = history(&temp);

    // The hook refuses it whole, as Artifacts would.
    let whole = Command::new("git")
        .args(["-C", repo.as_str(), "push", "-q", remote.as_str(), "main"])
        .output()
        .expect("git runs");
    assert!(!whole.status.success(), "the remote refuses it in one push");

    GitSource::push_leased(
        &["-C", repo.as_str()],
        remote.as_str(),
        &tip,
        "refs/heads/main",
        None,
        None,
    )
    .expect("pushed in steps");

    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), tip);
    assert_eq!(
        git(&remote, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/main",
        "the staging ref is gone"
    );
    assert_eq!(
        git(&remote, &["rev-list", "--count", "main"]),
        git(&repo, &["rev-list", "--count", "main"]),
        "every commit arrived"
    );
}

#[test]
fn a_push_the_remote_takes_whole_is_not_split() {
    let (_guard, temp) = tempdir();
    let remote = temp.join("open.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", remote.as_str()],
    );
    let (repo, tip) = history(&temp);

    GitSource::push_leased(
        &["-C", repo.as_str()],
        remote.as_str(),
        &tip,
        "refs/heads/main",
        None,
        None,
    )
    .expect("pushed");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), tip);
    assert_eq!(
        git(&remote, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/main"
    );
}

/// A bare remote that refuses a push bringing more than one file over 1 MiB
/// that it does not hold yet, as Artifacts refused a commit bringing two new
/// 24 MB binaries. Git sends a file again when only an unrelated history
/// reaches it, so what counts is what the remote already has, not what is
/// sent.
fn remote_refusing_large_pairs(temp: &Utf8Path) -> Utf8PathBuf {
    let remote = temp.join("pairs.git");
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
           large=0\n\
           for id in $(git rev-list --objects \"$new\" --not --all | cut -d' ' -f1); do\n\
             set -- $(git cat-file --batch-check='%(objecttype) %(objectsize)' <<EOF\n\
$id\n\
EOF\n\
             )\n\
             [ \"$1\" = blob ] && [ \"$2\" -gt 1048576 ] || continue\n\
             env -u GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_OBJECT_DIRECTORY=\"$PWD/objects\" \\\n\
               git cat-file -e \"$id\" 2>/dev/null && continue\n\
             large=$((large + 1))\n\
           done\n\
           if [ \"$large\" -gt 1 ]; then\n\
             echo \"remote: artifacts_git_receive_pack_failed\" >&2\n\
             exit 1\n\
           fi\n\
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

#[test]
fn a_commit_the_remote_will_not_take_has_its_large_files_sent_ahead() {
    let (_guard, temp) = tempdir();
    let remote = remote_refusing_large_pairs(&temp);
    let (repo, _) = history(&temp);
    // A commit bringing two large files at once, as t3code's libghostty
    // update did, with more history after it.
    let noise = |seed: u8| -> Vec<u8> {
        let mut state = u64::from(seed) + 1;
        (0..5 * 1024 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    };
    std::fs::write(repo.join("one.a"), noise(1)).expect("write");
    std::fs::write(repo.join("two.a"), noise(2)).expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "two large files"]);
    for n in 0..12 {
        std::fs::write(repo.join(format!("after{n}.txt")), "after\n").expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("after {n}")]);
    }
    let tip = git(&repo, &["rev-parse", "HEAD"]);

    GitSource::push_leased(
        &["-C", repo.as_str()],
        remote.as_str(),
        &tip,
        "refs/heads/main",
        None,
        None,
    )
    .expect("pushed, the large files ahead");

    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), tip);
    assert_eq!(
        git(&remote, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/main",
        "both staging refs are gone"
    );
    assert_eq!(
        git(&remote, &["rev-list", "--count", "main"]),
        git(&repo, &["rev-list", "--count", "main"]),
    );
}

/// What `newgit remote add` seeds a tracker with, and a checkpoint sync
/// sends: a plain push of a commit to a ref. Too big for the remote, it goes
/// in steps too.
#[test]
fn a_plain_push_too_big_for_the_remote_goes_in_steps() {
    let (_guard, temp) = tempdir();
    let remote = limited_remote(&temp);
    let (repo, tip) = history(&temp);

    GitSource::push_refs(
        &["-C", repo.as_str()],
        remote.as_str(),
        &[format!("+{tip}:refs/heads/main")],
        None,
    )
    .expect("pushed in steps");

    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), tip);
    assert_eq!(
        git(&remote, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/main",
        "the staging refs are gone"
    );
}
