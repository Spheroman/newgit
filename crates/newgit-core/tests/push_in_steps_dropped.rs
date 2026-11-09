//! A step the remote drops is a smaller step, or the same one again; never the
//! end of the push. Copying `t3code` into an instance's fork died 43 minutes
//! in when Artifacts answered one step with `The requested URL returned
//! error: 500` (`remote: Internal Server Error`), which newgit did not take
//! for a drop. A bare repository plays the remote: it refuses more than ten
//! new commits a push, as Artifacts refuses over 64 MiB, and drops every
//! third push it would otherwise take.
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
        "newgit-push-in-steps-dropped-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
    (TempDir(path.clone()), path)
}

/// Refuses more than ten new commits, and drops every third push it would
/// take, in the words Git prints for Artifacts' 500.
fn flaky_remote(temp: &Utf8Path) -> Utf8PathBuf {
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
         done\n\
         n=$(cat \"$GIT_DIR/pushes\" 2>/dev/null || echo 0)\n\
         n=$((n + 1))\n\
         echo \"$n\" > \"$GIT_DIR/pushes\"\n\
         if [ $((n % 3)) -eq 0 ]; then\n\
           echo \"Internal Server Error\" >&2\n\
           echo \"fatal: unable to access 'https://x.artifacts.cloudflare.net/git/x.git/': The requested URL returned error: 500\" >&2\n\
           exit 1\n\
         fi\n",
    )
    .expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    remote
}

/// Sixty commits in a line.
fn line(temp: &Utf8Path) -> (Utf8PathBuf, String) {
    let repo = temp.join("local");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for n in 0..60 {
        std::fs::write(repo.join(format!("file{n}.txt")), format!("{n}\n")).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("commit {n}")]);
    }
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    (repo, tip)
}

#[test]
fn a_copy_gets_through_a_remote_that_drops_steps() {
    let (_guard, temp) = tempdir();
    let remote = flaky_remote(&temp);
    let (repo, tip) = line(&temp);

    GitSource::push_refs(
        &["-C", repo.as_str()],
        remote.as_str(),
        &["+refs/heads/main:refs/heads/main".to_owned()],
        None,
    )
    .expect("pushed in steps through the drops");

    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), tip);
    assert_eq!(
        git(&remote, &["rev-list", "--count", "main"]),
        "60",
        "every commit arrived"
    );
}
