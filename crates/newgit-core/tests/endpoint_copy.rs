//! `newgit endpoint-copy`: the endpoint's container copies a tracker's repo
//! into an instance's empty fork, because Artifacts' own fork never finished
//! on Spheroman/t3code (365 MiB, #144). Bare repositories here play the
//! tracker and the fork; one fork has the `pre-receive` hook from
//! `push_in_steps.rs`, refusing a push of more than ten new commits as
//! Artifacts refuses one over 64 MiB.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::endpoint::{CopyPlan, Source, copy_repo};

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
    let dir = std::env::temp_dir().join(format!("newgit-endpoint-copy-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
    (TempDir(path.clone()), path)
}

fn bare(temp: &Utf8Path, name: &str) -> Utf8PathBuf {
    let repo = temp.join(name);
    git(temp, &["init", "-q", "--bare", "-b", "main", repo.as_str()]);
    repo
}

/// A fork that refuses a push bringing more than ten new commits, in
/// Artifacts' words.
fn limited_fork(temp: &Utf8Path) -> Utf8PathBuf {
    let remote = bare(temp, "fork.git");
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

/// The tracker's repo: forty commits on `main`, and a `side` branch.
fn tracker(temp: &Utf8Path) -> Utf8PathBuf {
    let work = temp.join("work");
    std::fs::create_dir_all(&work).expect("mkdir");
    git(&work, &["init", "-q", "-b", "main"]);
    for n in 0..40 {
        std::fs::write(work.join(format!("file{n}.txt")), format!("{n}\n")).expect("write");
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", &format!("commit {n}")]);
    }
    git(&work, &["checkout", "-q", "-b", "side", "HEAD~5"]);
    std::fs::write(work.join("side.txt"), "side\n").expect("write");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "side"]);
    let repo = bare(temp, "tracker.git");
    git(&work, &["push", "-q", repo.as_str(), "main", "side"]);
    repo
}

fn plan(temp: &Utf8Path, from: &Utf8Path, to: &Utf8Path) -> CopyPlan {
    let source = |url: &Utf8Path| Source {
        url: url.to_string(),
        auth: "Authorization: Basic eDpwdw==".to_owned(),
    };
    CopyPlan {
        dir: temp.join("container"),
        from: source(from),
        to: source(to),
    }
}

fn heads(repo: &Utf8Path) -> String {
    git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/",
        ],
    )
}

#[test]
fn copies_every_branch_into_an_empty_fork() {
    let (_guard, temp) = tempdir();
    let from = tracker(&temp);
    let to = bare(&temp, "open.git");
    let answer = copy_repo(&plan(&temp, &from, &to)).expect("copied");
    assert_eq!(heads(&to), heads(&from));
    assert_eq!(
        answer.branches.keys().collect::<Vec<_>>(),
        ["main", "side"],
        "the answer names every branch"
    );
    assert_eq!(
        git(&to, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/main\nrefs/heads/side",
        "nothing but the branches"
    );
    assert!(
        !temp.join("container/trackers.git").exists(),
        "a copy, shallow while it steps, stays out of the store merge checks read"
    );
}

#[test]
fn a_source_too_big_for_one_push_goes_in_steps() {
    let (_guard, temp) = tempdir();
    let from = tracker(&temp);
    let to = limited_fork(&temp);
    // The fork refuses it whole, as Artifacts would.
    let whole = Command::new("git")
        .args(["-C", from.as_str(), "push", "-q", to.as_str(), "main"])
        .output()
        .expect("git runs");
    assert!(!whole.status.success(), "the fork refuses it in one push");

    copy_repo(&plan(&temp, &from, &to)).expect("copied in steps");
    assert_eq!(heads(&to), heads(&from));
    assert_eq!(
        git(&to, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/main\nrefs/heads/side",
        "the staging refs are gone"
    );
    assert_eq!(
        git(&to, &["rev-list", "--count", "main"]),
        git(&from, &["rev-list", "--count", "main"]),
        "every commit arrived"
    );
}

#[test]
fn copying_again_after_a_partial_copy_finishes_it() {
    let (_guard, temp) = tempdir();
    let from = tracker(&temp);
    let to = limited_fork(&temp);
    // A copy that died part way: some of `main`, and no `side` at all.
    let partial = git(&from, &["rev-parse", "main~30"]);
    git(
        &from,
        &[
            "push",
            "-q",
            to.as_str(),
            &format!("{partial}:refs/heads/main"),
        ],
    );

    let plan = plan(&temp, &from, &to);
    copy_repo(&plan).expect("finished");
    assert_eq!(heads(&to), heads(&from));

    // And again, with nothing left to do: the same answer, nothing moved.
    let again = copy_repo(&plan).expect("copied again");
    assert_eq!(heads(&to), heads(&from));
    assert_eq!(again.branches.len(), 2);
}
