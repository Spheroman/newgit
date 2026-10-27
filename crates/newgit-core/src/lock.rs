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
//! A hook run under the lock is told so through `NEWGIT_LOCKED`, and a
//! `newgit` it starts that would need the same lock fails at once rather
//! than waiting for its own ancestor forever.

use std::fs::{File, TryLockError};

use camino::Utf8Path;
use nix::sys::signal::kill;
use nix::unistd::Pid;

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;

/// The environment variable a locked command sets for the hooks it runs:
/// `<slug>:<pid>`, the instance locked and the process holding it.
pub const LOCKED_ENV: &str = "NEWGIT_LOCKED";

/// An instance's lock, held until dropped.
#[derive(Debug)]
pub struct InstanceLock {
    slug: String,
    // Holding the file open is holding the lock; closing it releases.
    _file: File,
}

impl InstanceLock {
    /// Take `slug`'s lock, waiting for whoever holds it now. `name` is only
    /// for the message printed while waiting.
    pub(crate) fn acquire(dir: &Utf8Path, slug: &str, name: &str) -> Result<Self> {
        create_dir_all(dir)?;
        let path = dir.join(format!("{slug}.lock"));
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
                if let Some(holder) = ancestor_holding(slug) {
                    return Err(NewgitError::NestedCommand {
                        instance: name.to_owned(),
                        pid: holder,
                    });
                }
                eprintln!("newgit: waiting for `{name}`: another newgit command is running on it");
                file.lock()
                    .map_err(|source| NewgitError::io(&path, source))?;
            }
            Err(TryLockError::Error(source)) => return Err(NewgitError::io(&path, source)),
        }
        Ok(Self {
            slug: slug.to_owned(),
            _file: file,
        })
    }

    pub fn slug(&self) -> &str {
        &self.slug
    }
}

/// The pid of a still-running ancestor that holds `slug`'s lock, read from
/// the environment it gave this process. A dead pid is a hook that outlived
/// the command which started it (a server, say) and means nothing now.
fn ancestor_holding(slug: &str) -> Option<u32> {
    let value = std::env::var(LOCKED_ENV).ok()?;
    let (locked, pid) = value.rsplit_once(':')?;
    if locked != slug {
        return None;
    }
    let pid: u32 = pid.parse().ok()?;
    let alive = kill(Pid::from_raw(i32::try_from(pid).ok()?), None).is_ok();
    alive.then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[test]
    fn a_second_open_of_the_same_lock_waits_for_the_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let first = InstanceLock::acquire(&dir, "x", "x").expect("first");
        assert_eq!(first.slug(), "x");

        let path = dir.join("x.lock");
        let probe = File::open(&path).expect("open");
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
        drop(first);
        assert!(probe.try_lock().is_ok());
    }

    #[test]
    fn locks_of_different_instances_do_not_wait_for_each_other() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let _x = InstanceLock::acquire(&dir, "x", "x").expect("x");
        let _y = InstanceLock::acquire(&dir, "y", "y").expect("y");
    }
}
