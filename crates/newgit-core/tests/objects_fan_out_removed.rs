//! A loose object written while Git removes its fan-out directory (#475).
//!
//! `git repack -d` (which `git gc` and the detached `git maintenance run
//! --auto` that `git commit` starts both run) prunes the loose objects it
//! packed and then removes every fan-out directory, `rmdir` on each of
//! `objects/00` to `objects/ff`, failing harmlessly on any not empty. A
//! write in process makes its temporary file in that directory, so it can
//! find the directory gone just as it creates the file there. Linux says
//! `ENOENT`; macOS says `ENOENT` or, when the `rmdir` lands between looking
//! the directory up and making the file, `EINVAL`. Either way the write
//! makes the directory again and tries again, as Git's own
//! `create_tmpfile` does for `ENOENT`.
//!
//! The first test fails every run without that (the writer used to retry
//! `ENOENT` once and stop at `EINVAL`); the second, where the directory is
//! also made again by someone else, is what tells an `EINVAL` from a
//! removal by the directory's identity rather than by whether it is there.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::objects::Objects;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Every fan-out directory removed, over and over, as `prune-packed` does
/// it, until `stop`; how many removals took. With `remake`, each is made
/// again straight after, as another writer (Git's own `create_tmpfile`)
/// would.
fn prune_fan_outs(
    objects: Utf8PathBuf,
    remake: bool,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<usize> {
    let removed = AtomicUsize::new(0);
    std::thread::spawn(move || {
        let dirs: Vec<Utf8PathBuf> = (0..=255u8)
            .map(|byte| objects.join(format!("{byte:02x}")))
            .collect();
        while !stop.load(Ordering::Relaxed) {
            for dir in &dirs {
                if std::fs::remove_dir(dir).is_ok() {
                    removed.fetch_add(1, Ordering::Relaxed);
                }
                if remake {
                    let _ = std::fs::create_dir(dir);
                }
            }
        }
        removed.into_inner()
    })
}

#[test]
fn objects_written_while_git_removes_fan_out_directories() {
    write_while_pruned(false);
}

#[test]
fn objects_written_while_git_removes_and_another_writer_remakes_them() {
    write_while_pruned(true);
}

fn write_while_pruned(remake: bool) {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonical"))
        .expect("utf8")
        .join("repo.git");
    git(
        repo.parent().expect("parent"),
        &["init", "-q", "--bare", repo.as_str()],
    );
    let stop = Arc::new(AtomicBool::new(false));
    let pruner = prune_fan_outs(repo.join("objects"), remake, Arc::clone(&stop));

    let mut objects = Objects::open(&repo).expect("open");
    let mut written = Vec::new();
    let mut failure = None;
    'rounds: for round in 0..40 {
        for n in 0..250 {
            written.push(objects.make_blob(format!("round {round}, blob {n}\n").as_bytes()));
        }
        if let Err(error) = objects.flush() {
            failure = Some(format!("round {round}: {error}"));
            break 'rounds;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let removed = pruner.join().expect("pruner");
    assert_eq!(
        failure, None,
        "a write failed while Git removed fan-out directories"
    );
    assert!(
        removed > 0,
        "the fan-out directories were never removed, so this tested nothing"
    );

    // Each is there and is what Git would have written. The ids go in
    // from a file: through a pipe, they would fill Git's output before
    // they were all in.
    let ids = temp.path().join("ids");
    std::fs::write(&ids, written.join("\n") + "\n").expect("write");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo.as_str())
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(std::fs::File::open(&ids).expect("open"))
        .output()
        .expect("git runs");
    assert!(output.status.success());
    let found = String::from_utf8_lossy(&output.stdout);
    assert_eq!(found.lines().count(), written.len());
    for (line, id) in found.lines().zip(&written) {
        assert_eq!(line, format!("{id} blob"));
    }
    git(&repo, &["fsck", "--strict", "--no-dangling"]);
}
