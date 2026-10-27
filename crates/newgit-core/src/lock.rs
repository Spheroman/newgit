//! One lock per branch instance, held by a command from start to finish.
//!
//! Every command that changes an instance — its binding record, its
//! checkpoint log, the lanes and resources those describe — loads the record,
//! works, and saves it back. Two of them overlapping used to lose one's work
//! (#212): both minted the same checkpoint id, or the one finishing last
//! saved a record it loaded before the other had changed it. The lock makes
//! the rule simple: *a command holds its instance's lock for as long as it
//! runs, and a second command on the same instance waits.*
//!
//! It is an advisory file lock (`flock`) on `.newgit/local/locks/<slug>.lock`.
//! The kernel drops it when the holding process exits, however it exits, so
//! a crash leaves nothing to clean up and there is no such thing as a stale
//! lock. A process that holds it cannot block itself: the manager hands the
//! same held lock back to a nested call (see `BranchManager::lock_instance`).
//!
//! **Nested commands.** A hook run under the lock may itself run `newgit` on
//! the same instance — a `[checkpoint] command` running `newgit checkpoint`.
//! That one would wait for its own ancestor forever, so it has to be told
//! apart from a command that should simply wait. Whoever takes a lock writes
//! a fresh token into the lock file, and a hook run under it gets
//! `NEWGIT_LOCKED=<slug>:<token>` — one entry per lock held, appended to any
//! it inherited, so a hook on `x` running `newgit` on `y` whose hook runs
//! `newgit` on `x` is still caught. A `newgit` that finds a lock busy refuses
//! only when the token the file holds *now* is one in its environment: the
//! token is cleared when the lock is released and a new holder writes its
//! own, so a dev server that outlived the command which started it carries a
//! token that matches nothing and waits like anyone else.
//!
//! What this does not catch: two *independent* commands whose hooks each run
//! `newgit` on the other's instance (one holds `x` and its hook wants `y`,
//! the other holds `y` and its hook wants `x`). Neither is the other's
//! ancestor; each waits, printing that it is, until one is interrupted.
//!
//! **Lock files.** A lock file is left in place when its lock is released.
//! `cleanup` deletes those of instances that no longer have a record. That is
//! safe only because acquiring checks, once it holds the lock, that the file
//! it locked is still the one at the path — one deleted while it waited is
//! not the lock any more, and it starts over.

use std::fs::{File, TryLockError};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;
use crate::store::read_dir_sorted;

/// The environment variable a locked command sets for the hooks it runs: a
/// comma-separated list of `<slug>:<token>`, one per instance lock held by
/// this command or one it was started from.
pub const LOCKED_ENV: &str = "NEWGIT_LOCKED";

/// An instance's lock, held until dropped.
#[derive(Debug)]
pub struct InstanceLock {
    slug: String,
    token: String,
    // Holding the file open is holding the lock; closing it releases.
    file: File,
}

impl InstanceLock {
    /// Take `slug`'s lock, waiting for whoever holds it now. `name` is only
    /// for the messages.
    pub(crate) fn acquire(dir: &Utf8Path, slug: &str, name: &str) -> Result<Self> {
        create_dir_all(dir)?;
        let path = lock_path(dir, slug);
        let mut announced = false;
        loop {
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|source| NewgitError::io(&path, source))?;
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    if let Some(pid) = held_by_ancestor(&path, slug) {
                        return Err(NewgitError::NestedCommand {
                            instance: name.to_owned(),
                            pid,
                        });
                    }
                    if !announced {
                        eprintln!(
                            "newgit: waiting for `{name}`: another newgit command is running on it"
                        );
                        announced = true;
                    }
                    file.lock()
                        .map_err(|source| NewgitError::io(&path, source))?;
                }
                Err(TryLockError::Error(source)) => return Err(NewgitError::io(&path, source)),
            }
            // `cleanup` deleted the file while this waited on it: whoever
            // opens the path now gets a different file, so this one is no
            // longer the lock.
            if !is_file_at(&file, &path)? {
                continue;
            }
            let token = new_token();
            let mut writer = &file;
            file.set_len(0)
                .and_then(|()| writer.write_all(token.as_bytes()))
                .map_err(|source| NewgitError::io(&path, source))?;
            return Ok(Self {
                slug: slug.to_owned(),
                token,
                file,
            });
        }
    }

    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// What the lock file holds while this lock is held, and what hooks run
    /// under it find in `NEWGIT_LOCKED`.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Cleared before the file closes and the lock goes with it, so a
        // token a hook carries matches the file only while this is held.
        let _ = self.file.set_len(0);
    }
}

/// `NEWGIT_LOCKED` for a hook run while this process holds `held`
/// (`(slug, token)` pairs): what this process inherited, with an entry for
/// each lock it holds replacing any inherited one for the same instance.
pub(crate) fn locked_env_value<'a>(held: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let held: Vec<(&str, &str)> = held.into_iter().collect();
    let inherited = std::env::var(LOCKED_ENV).unwrap_or_default();
    let mut entries: Vec<String> = parse_entries(&inherited)
        .filter(|(slug, _)| !held.iter().any(|(held, _)| held == slug))
        .map(|(slug, token)| format!("{slug}:{token}"))
        .collect();
    entries.extend(held.iter().map(|(slug, token)| format!("{slug}:{token}")));
    entries.join(",")
}

/// Delete the lock files of instances `keep` does not name, skipping any
/// held right now. Returns what was deleted.
pub(crate) fn prune(dir: &Utf8Path, keep: impl Fn(&str) -> bool) -> Result<Vec<Utf8PathBuf>> {
    let mut pruned = Vec::new();
    for path in read_dir_sorted(dir)? {
        if path.extension() != Some("lock") {
            continue;
        }
        let Some(slug) = path.file_stem() else {
            continue;
        };
        if keep(slug) {
            continue;
        }
        let file = match File::options().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(source) => return Err(NewgitError::io(&path, source)),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(source)) => return Err(NewgitError::io(&path, source)),
        }
        // Removed while this holds it, so a command that opened it and is
        // waiting finds it gone once it gets the lock, and starts over.
        match std::fs::remove_file(&path) {
            Ok(()) => pruned.push(path),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(source) => return Err(NewgitError::io(&path, source)),
        }
    }
    Ok(pruned)
}

fn lock_path(dir: &Utf8Path, slug: &str) -> Utf8PathBuf {
    dir.join(format!("{slug}.lock"))
}

/// Whether `file` is still the file at `path`.
fn is_file_at(file: &File, path: &Utf8Path) -> Result<bool> {
    let at_path = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(NewgitError::io(path, source)),
    };
    let open = file
        .metadata()
        .map_err(|source| NewgitError::io(path, source))?;
    Ok(at_path.dev() == open.dev() && at_path.ino() == open.ino())
}

/// Unique to one acquisition, and naming the process that made it: the pid,
/// then a nanosecond clock and a counter so a reused pid never repeats one.
fn new_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}.{nanos}.{count}", std::process::id())
}

fn parse_entries(value: &str) -> impl Iterator<Item = (&str, &str)> {
    value
        .split(',')
        .filter_map(|entry| entry.trim().split_once(':'))
}

/// The pid holding `slug`'s lock, when that holder is a command this process
/// was started from: the token in the lock file right now is one this
/// process inherited.
fn held_by_ancestor(path: &Utf8Path, slug: &str) -> Option<u32> {
    let inherited = std::env::var(LOCKED_ENV).ok()?;
    let (_, token) = parse_entries(&inherited).find(|(locked, _)| *locked == slug)?;
    let current = std::fs::read_to_string(path).ok()?;
    if token.is_empty() || current.trim() != token {
        return None;
    }
    token.split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        (temp, dir)
    }

    #[test]
    fn a_second_open_of_the_same_lock_waits_for_the_first() {
        let (_temp, dir) = dir();
        let first = InstanceLock::acquire(&dir, "x", "x").expect("first");
        assert_eq!(first.slug(), "x");

        let path = dir.join("x.lock");
        let probe = File::open(&path).expect("open");
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
        drop(first);
        assert!(try_lock_soon(&probe));
    }

    /// `try_lock`, retried briefly: another test in this process spawning a
    /// command can hold a copy of a lock's descriptor between its fork and
    /// exec (the descriptor is close-on-exec, so only that long).
    fn try_lock_soon(file: &File) -> bool {
        let start = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return true,
                Err(_) if start.elapsed() < std::time::Duration::from_secs(2) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => return false,
            }
        }
    }

    #[test]
    fn locks_of_different_instances_do_not_wait_for_each_other() {
        let (_temp, dir) = dir();
        let _x = InstanceLock::acquire(&dir, "x", "x").expect("x");
        let _y = InstanceLock::acquire(&dir, "y", "y").expect("y");
    }

    #[test]
    fn the_lock_file_holds_the_token_only_while_the_lock_is_held() {
        let (_temp, dir) = dir();
        let lock = InstanceLock::acquire(&dir, "x", "x").expect("x");
        let token = lock.token().to_owned();
        assert!(token.starts_with(&format!("{}.", std::process::id())));
        assert_eq!(
            std::fs::read_to_string(dir.join("x.lock")).expect("read"),
            token
        );
        drop(lock);
        assert_eq!(
            std::fs::read_to_string(dir.join("x.lock")).expect("read"),
            ""
        );

        let again = InstanceLock::acquire(&dir, "x", "x").expect("again");
        assert_ne!(again.token(), token);
    }

    #[test]
    fn pruning_skips_a_held_lock_and_removes_an_idle_one() {
        let (_temp, dir) = dir();
        let held = InstanceLock::acquire(&dir, "held", "held").expect("held");
        drop(InstanceLock::acquire(&dir, "idle", "idle").expect("idle"));
        drop(InstanceLock::acquire(&dir, "kept", "kept").expect("kept"));

        // Retried for the reason `try_lock_soon` is.
        let start = std::time::Instant::now();
        let mut pruned = prune(&dir, |slug| slug == "kept").expect("prune");
        while pruned.is_empty() && start.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(10));
            pruned = prune(&dir, |slug| slug == "kept").expect("prune");
        }
        assert_eq!(pruned, vec![dir.join("idle.lock")]);
        assert!(dir.join("held.lock").exists());
        assert!(dir.join("kept.lock").exists());
        drop(held);
    }

    /// A command waiting on a lock file `cleanup` deletes takes the lock on
    /// the file at the path afterwards, not the deleted one — otherwise it
    /// and a third command could both hold "the" lock.
    #[test]
    fn a_lock_whose_file_was_pruned_while_waiting_starts_over() {
        let (_temp, dir) = dir();
        let path = dir.join("x.lock");
        let first = InstanceLock::acquire(&dir, "x", "x").expect("first");

        let waiter = {
            let dir = dir.clone();
            std::thread::spawn(move || InstanceLock::acquire(&dir, "x", "x").expect("waiter"))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        std::fs::remove_file(&path).expect("prune");
        drop(first);

        let waiter = waiter.join().expect("thread");
        assert!(is_file_at(&waiter.file, &path).expect("stat"));
        let probe = File::open(&path).expect("open");
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
    }
}
