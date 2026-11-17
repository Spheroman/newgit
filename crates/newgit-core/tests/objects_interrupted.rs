//! A write of loose objects stopped part way (a crash) leaves a repository
//! that has every object reachable from each object it has (#460 review):
//! each goes into place only after everything it names. So a copy that
//! stops at what a repository has, as the next one after a crash does,
//! leaves nothing missing.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::objects::{self, Entry, Objects};

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {dir}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn bare(temp: &Utf8Path, name: &str) -> Utf8PathBuf {
    let dir = temp.join(name);
    let status = Command::new("git")
        .args(["init", "-q", "--bare", dir.as_str()])
        .status()
        .expect("git runs");
    assert!(status.success());
    dir
}

const BY: &[u8] = b"t <t@example.com> 1 +0000";

/// Two commits, each adding a file under `docs/`, made in `objects`
/// (not flushed): the second's id.
fn history(objects: &mut Objects) -> String {
    let mut parents = Vec::new();
    let mut entries = Vec::new();
    for name in ["a.md", "b.md"] {
        entries.push(Entry {
            mode: b"100644".to_vec(),
            name: name.as_bytes().to_vec(),
            id: objects.make_blob(format!("{name}\n").as_bytes()),
        });
        let docs = objects.make_tree(entries.clone());
        let tree = objects.make_tree(vec![Entry {
            mode: b"40000".to_vec(),
            name: b"docs".to_vec(),
            id: docs,
        }]);
        let commit = objects.make_commit(&tree, &parents, BY, BY, b"", name.as_bytes());
        parents = vec![commit];
    }
    parents.remove(0)
}

/// Every object in `dir`, named by a ref of its own, passes `fsck`: each
/// it has reaches only objects it has.
fn every_object_complete(dir: &Utf8Path) {
    let all = git(
        dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    for id in all.lines() {
        git(dir, &["update-ref", &format!("refs/keep/{id}"), id]);
    }
    git(dir, &["fsck", "--strict", "--no-dangling"]);
}

/// How many objects `history` makes: two commits, two root trees, two
/// `docs` trees, two blobs.
const MADE: usize = 8;

#[test]
fn a_flush_stopped_part_way_leaves_only_complete_objects() {
    for renames in 0..MADE {
        let temp = tempfile::tempdir().expect("tempdir");
        let temp = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf-8");
        let dir = bare(&temp, "a.git");
        let mut objects = Objects::open(&dir).expect("open");
        history(&mut objects);
        objects::stop_writes_after(Some(renames));
        let stopped = objects.flush();
        objects::stop_writes_after(None);
        assert!(stopped.is_err(), "stopped after {renames}");
        every_object_complete(&dir);
    }
}

#[test]
fn a_copy_after_one_stopped_part_way_leaves_nothing_missing() {
    for renames in 0..MADE {
        let temp = tempfile::tempdir().expect("tempdir");
        let temp = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf-8");
        let from_dir = bare(&temp, "from.git");
        let into_dir = bare(&temp, "into.git");
        let mut from = Objects::open(&from_dir).expect("open");
        let tip = history(&mut from);
        from.flush().expect("flush");

        let mut into = Objects::open(&into_dir).expect("open");
        objects::stop_writes_after(Some(renames));
        let stopped = into.copy_reachable(&mut from, &tip);
        objects::stop_writes_after(None);
        assert!(stopped.is_err(), "stopped after {renames}");
        every_object_complete(&into_dir);

        // The next copy, as a placement after the crash: none when the tip
        // is there (it has everything), the rest otherwise.
        if !into.contains(&tip).expect("asks") {
            let copied = into.copy_reachable(&mut from, &tip).expect("copy");
            assert_eq!(copied, Some(MADE - renames), "after {renames}");
        }
        git(&into_dir, &["update-ref", "refs/heads/main", &tip]);
        git(&into_dir, &["fsck", "--strict", "--no-dangling"]);
    }
}

#[test]
fn a_copy_over_parents_waits_for_them() {
    let temp = tempfile::tempdir().expect("tempdir");
    let temp = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf-8");
    let from_dir = bare(&temp, "from.git");
    let into_dir = bare(&temp, "into.git");
    let mut from = Objects::open(&from_dir).expect("open");
    let tip = history(&mut from);
    from.flush().expect("flush");
    let parents = from.commit(&tip).expect("tip").parents;
    let mut into = Objects::open(&into_dir).expect("open");
    // Without the parent, its blob (which the tip's tree also names) could
    // cross: nothing is copied.
    assert_eq!(
        into.copy_reachable_over(&mut from, &tip, &parents)
            .expect("copy"),
        None
    );
    assert_eq!(git(&into_dir, &["count-objects"]), "0 objects, 0 kilobytes");
    // With it, only what the tip adds: itself, two trees, a blob.
    assert_eq!(
        into.copy_reachable(&mut from, &parents[0]).expect("copy"),
        Some(4)
    );
    assert_eq!(
        into.copy_reachable_over(&mut from, &tip, &parents)
            .expect("copy"),
        Some(4)
    );
    git(&into_dir, &["update-ref", "refs/heads/main", &tip]);
    git(&into_dir, &["fsck", "--strict", "--no-dangling"]);
}
