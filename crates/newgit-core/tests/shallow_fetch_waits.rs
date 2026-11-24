//! A fetch into a store that is already shallow deepens it from its very
//! first, whole fetch, so it takes the stepped-fetch lock before that one
//! too, and waits out whoever holds it — another stepped fetch, or a `gc`
//! (#441). The lock is held from the test, as either would hold it.

use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::source::GitSource;

fn git(args: &[&str]) {
    let output = Command::new("git")
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
}

/// A history of a few commits, and a store cloned from it one commit deep.
fn shallow_store() -> (tempfile::TempDir, Utf8PathBuf, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = Utf8Path::from_path(temp.path()).expect("utf8").to_owned();
    let origin = root.join("origin");
    git(&["init", "-q", "-b", "main", origin.as_str()]);
    for n in 0..4 {
        git(&[
            "-C",
            origin.as_str(),
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("commit {n}"),
        ]);
    }
    let url = format!("file://{origin}");
    let store = root.join("store.git");
    git(&["clone", "-q", "--bare", "--depth=1", &url, store.as_str()]);
    assert!(store.join("shallow").is_file(), "the store starts shallow");
    // And one more, for the fetch to bring.
    git(&[
        "-C",
        origin.as_str(),
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "the one to fetch",
    ]);
    (temp, store, url)
}

fn main_in(store: &Utf8Path) -> String {
    let output = Command::new("git")
        .args(["--git-dir", store.as_str(), "rev-parse", "refs/heads/main"])
        .output()
        .expect("git runs");
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

#[test]
fn a_fetch_into_a_shallow_store_waits_for_the_stepped_fetch_lock() {
    let (_temp, store, url) = shallow_store();
    let before = main_in(&store);
    let lock = std::fs::File::create(store.join("newgit-steps.lock")).expect("lock file");
    lock.lock().expect("lock");

    let (done, finished) = mpsc::channel();
    let fetching = std::thread::spawn({
        let store = store.clone();
        move || {
            let location = format!("--git-dir={store}");
            let result = GitSource::fetch_refs(
                &[location.as_str()],
                &url,
                &["+refs/heads/*:refs/heads/*".to_owned()],
                None,
            );
            done.send(()).expect("receiver is waiting");
            result
        }
    });

    assert!(
        finished.recv_timeout(Duration::from_millis(700)).is_err(),
        "the fetch finished while another held the stepped-fetch lock"
    );
    assert_eq!(
        main_in(&store),
        before,
        "the opening fetch ran into the shallow store while another held the lock"
    );
    drop(lock);
    finished
        .recv_timeout(Duration::from_secs(60))
        .expect("the fetch carries on once the lock is let go");
    fetching
        .join()
        .expect("fetch thread")
        .expect("the fetch succeeds");
    assert!(!store.join("shallow").is_file(), "the store is whole");
}
