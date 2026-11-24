//! `newgit gc` waits out a stepped fetch into the tracker store (#441).
//!
//! A stepped fetch holds `shallow.lock` for each band and a `git gc` meeting
//! it dies with "failed to run repack". The fetch also holds the store's
//! stepped-fetch lock for its whole run, so the prune takes that first:
//! these hold it from the test, the way a deepening fetch would, and check
//! the prune neither runs under it nor fails on it.

use std::sync::mpsc;
use std::time::Duration;

use camino::Utf8PathBuf;
use newgit_core::tracker_repo::{ensure_store, prune_unreferenced};

fn store() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().join("trackers.git")).expect("utf8");
    ensure_store(&path).expect("store");
    (temp, path)
}

/// What a stepped fetch holds: an exclusive lock on this file, in the store.
fn hold_steps(store: &Utf8PathBuf) -> std::fs::File {
    let lock = std::fs::File::create(store.join("newgit-steps.lock")).expect("lock file");
    lock.lock().expect("lock");
    lock
}

#[test]
fn prune_waits_for_a_stepped_fetch_to_finish() {
    let (_temp, store) = store();
    let steps = hold_steps(&store);

    let (done, finished) = mpsc::channel();
    let pruning = std::thread::spawn({
        let store = store.clone();
        move || {
            let result = prune_unreferenced(&store);
            done.send(()).expect("receiver is waiting");
            result
        }
    });

    assert!(
        finished.recv_timeout(Duration::from_millis(700)).is_err(),
        "the prune ran while a stepped fetch held the store"
    );
    drop(steps);
    finished
        .recv_timeout(Duration::from_secs(60))
        .expect("the prune carries on once the fetch lets go");
    pruning
        .join()
        .expect("prune thread")
        .expect("the prune succeeds");
}

#[test]
fn prune_with_no_fetch_running_does_not_wait() {
    let (_temp, store) = store();
    prune_unreferenced(&store).expect("prune");
    // Leaves the lock free for the next stepped fetch.
    let _again = hold_steps(&store);
}
