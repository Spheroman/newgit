//! A stepped fetch shares its store with other Git processes that rewrite
//! the store's `shallow` file, and finishes anyway (#432).
//!
//! Git's auto-maintenance, detached after every fetch, runs `git repack`,
//! which rewrites `shallow` under `shallow.lock` in a shallow repository.
//! A fetch deepening the store meanwhile fails with `shallow file has
//! changed since we read it`, or on finding `shallow.lock` taken. No newgit
//! lock covers Git's own processes, so the fetch is tried again. Here a
//! thread stands in for that repack, doing what `prune_shallow` does: take
//! `shallow.lock`, write the same commits, rename it over `shallow`.
//!
//! A local remote serves packs through `uploadpack.packObjectsHook`, as in
//! `fetch_in_steps.rs`, failing any pack over 40 KiB, so the fetch goes in
//! many bands.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "newgit-fetch-beside-git-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
    (TempDir(path.clone()), path)
}

/// Sixty commits rewriting one file with incompressible bytes: five times
/// what the remote below sends at once.
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

/// A bare store with `upstream` as remote `origin`, served through a hook
/// that fails any pack over 40 KiB when `limited`.
fn store(temp: &Utf8Path, upstream: &Utf8Path, limited: bool) -> Utf8PathBuf {
    let store = temp.join("store.git");
    git(temp, &["init", "-q", "--bare", store.as_str()]);
    git(
        &store,
        &["config", "remote.origin.url", &format!("file://{upstream}")],
    );
    if limited {
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
        git(
            &store,
            &[
                "config",
                "remote.origin.uploadpack",
                &format!("git -c uploadpack.packObjectsHook={hook} upload-pack"),
            ],
        );
    }
    store
}

fn fetch(store: &Utf8Path) -> newgit_core::error::Result<()> {
    GitSource::fetch_refs(
        &[&format!("--git-dir={store}")],
        "origin",
        &["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        None,
    )
}

fn assert_whole(store: &Utf8Path, tip: &str) {
    assert_eq!(git(store, &["rev-parse", "refs/remotes/origin/main"]), tip);
    assert_eq!(
        git(store, &["rev-parse", "--is-shallow-repository"]),
        "false",
        "the history whole"
    );
    assert_eq!(
        git(store, &["rev-list", "--count", "refs/remotes/origin/main"]),
        "60"
    );
}

/// What `prune_shallow` does once: take `shallow.lock`, hold it for `hold`
/// (the repack's work), and rename the same commits over `shallow`. False
/// when Git holds the lock, or the store is no longer shallow.
fn rewrite_shallow(store: &Utf8Path, hold: Duration) -> bool {
    let lock = store.join("shallow.lock");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    else {
        return false;
    };
    let Ok(commits) = std::fs::read(store.join("shallow")) else {
        let _ = std::fs::remove_file(&lock);
        return false;
    };
    std::thread::sleep(hold);
    use std::io::Write as _;
    file.write_all(&commits).expect("write shallow.lock");
    drop(file);
    std::fs::rename(&lock, store.join("shallow")).expect("rename over shallow");
    true
}

#[test]
fn a_stepped_fetch_waits_for_another_git_holding_the_shallow_lock() {
    let (_guard, temp) = tempdir();
    let (upstream, tip) = upstream(&temp);
    let store = store(&temp, &upstream, false);
    // A stepped fetch that stopped part way left the store shallow; the
    // next fetch deepens it to the end.
    git(
        &store,
        &[
            "fetch",
            "-q",
            "--depth=1",
            "--no-auto-maintenance",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    // Another Git is part way through rewriting `shallow`.
    let lock = store.join("shallow.lock");
    std::fs::write(&lock, "").expect("take shallow.lock");

    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(1500));
            std::fs::remove_file(&lock).expect("release shallow.lock");
        });
        fetch(&store)
    });

    result.expect("fetched once the other Git let go");
    assert_whole(&store, &tip);
}

#[test]
fn a_stepped_fetch_finishes_while_another_git_rewrites_shallow() {
    for _ in 0..3 {
        let (_guard, temp) = tempdir();
        let (upstream, tip) = upstream(&temp);
        let store = store(&temp, &upstream, true);
        let done = AtomicBool::new(false);

        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                // From the first band on, a repack a tenth of a second,
                // every tenth of a second, for a while.
                while !done.load(Ordering::Relaxed) && !store.join("shallow").exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let mut rewrites = 0;
                while !done.load(Ordering::Relaxed) && rewrites < 15 {
                    // Long holds meet a band at the lock; short ones swap
                    // the file between its reading and its writing.
                    let hold = if rewrites % 2 == 0 { 100 } else { 2 };
                    if rewrite_shallow(&store, Duration::from_millis(hold)) {
                        rewrites += 1;
                    }
                    std::thread::sleep(Duration::from_millis(30));
                }
            });
            let result = fetch(&store);
            done.store(true, Ordering::Relaxed);
            result
        });

        result.expect("fetched beside the other Git");
        assert_whole(&store, &tip);
    }
}
