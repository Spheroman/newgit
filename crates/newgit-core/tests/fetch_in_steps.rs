//! A fetch too big for the remote to send at once goes in steps. Artifacts
//! fails a fetch of all of Spheroman/t3code (365 MiB) with an HTTP 500 and
//! `fatal: expected 'packfile'`, which stopped the remote from checking a
//! merge that brings t3code in (#144). Here a local remote serves packs
//! through `uploadpack.packObjectsHook`, which fails any pack over 40 KiB.
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
    let dir = std::env::temp_dir().join(format!("newgit-fetch-steps-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
    (TempDir(path.clone()), path)
}

/// Sixty commits, each rewriting one file with a few KiB of incompressible
/// content: a small snapshot over about 200 KiB of history, five times what
/// the remote below sends at once, as t3code's history dwarfs its tree.
fn upstream(temp: &Utf8Path) -> (Utf8PathBuf, String) {
    let repo = temp.join("upstream");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    let mut state: u64 = 7;
    for n in 0..60 {
        let bytes: Vec<u8> = (0..3500)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        std::fs::write(repo.join("file.bin"), bytes).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("commit {n}")]);
    }
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    (repo, tip)
}

/// A hook that serves a pack only when it is under 40 KiB, as Artifacts
/// serves only so much at once.
fn limiting_hook(temp: &Utf8Path) -> Utf8PathBuf {
    let hook = temp.join("pack-hook.sh");
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         out=$(mktemp)\n\
         \"$@\" > \"$out\" || exit 1\n\
         size=$(wc -c < \"$out\")\n\
         if [ \"$size\" -gt 40960 ]; then rm -f \"$out\"; exit 1; fi\n\
         cat \"$out\"; rm -f \"$out\"\n",
    )
    .expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    hook
}

#[test]
fn a_fetch_too_big_for_the_remote_goes_in_steps() {
    let (_guard, temp) = tempdir();
    let (upstream, tip) = upstream(&temp);
    let hook = limiting_hook(&temp);
    let store = temp.join("store.git");
    git(&temp, &["init", "-q", "--bare", store.as_str()]);
    // The hook is the serving side's, and Git takes it only from the global
    // config or upload-pack's own command line (a local fetch clears what
    // `-c` passes). So the fetch goes through a named remote whose
    // `uploadpack` starts upload-pack with the hook.
    let git_dir = format!("--git-dir={store}");
    git(
        &store,
        &["config", "remote.up.url", &format!("file://{upstream}")],
    );
    git(
        &store,
        &[
            "config",
            "remote.up.uploadpack",
            &format!("git -c uploadpack.packObjectsHook={hook} upload-pack"),
        ],
    );
    let url = "up".to_owned();
    let location = [git_dir.as_str()];

    // Whole, the remote will not send it.
    let whole = Command::new("git")
        .args(location)
        .args(["fetch", "-q", "--", &url, "+refs/heads/*:refs/up/*"])
        .output()
        .expect("git runs");
    assert!(!whole.status.success(), "the remote refuses it whole");

    GitSource::fetch_refs(
        &location,
        &url,
        &["+refs/heads/*:refs/up/*".to_owned()],
        None,
    )
    .expect("fetched in steps");

    assert_eq!(git(&store, &["rev-parse", "refs/up/main"]), tip);
    assert_eq!(
        git(&store, &["rev-parse", "--is-shallow-repository"]),
        "false",
        "the whole history, not a shallow one"
    );
    assert_eq!(git(&store, &["rev-list", "--count", "refs/up/main"]), "60");
    git(&store, &["fsck", "--connectivity-only"]);
}
