//! A flush of thousands of objects keeps few files open (#460 review): each
//! staged object's file is closed once written, so the process's limit on
//! open files (256 by default on macOS) is never what limits a flush.
//!
//! Its own test binary, as it lowers the whole process's limit.

use std::process::Command;

use camino::Utf8PathBuf;
use newgit_core::objects::{Entry, Objects};
use nix::sys::resource::{Resource, getrlimit, setrlimit};

const MANY: usize = 3000;

#[test]
fn a_flush_of_thousands_of_objects_fits_a_low_open_file_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(temp.path().join("a.git")).expect("utf-8");
    let init = Command::new("git")
        .args(["init", "-q", "--bare", dir.as_str()])
        .status()
        .expect("git runs");
    assert!(init.success());

    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).expect("getrlimit");
    setrlimit(Resource::RLIMIT_NOFILE, 256.min(hard), hard).expect("setrlimit");

    let mut objects = Objects::open(&dir).expect("open");
    // Synced, so the device's flush (which keeps one file) runs too.
    objects.set_durable(true);
    let entries: Vec<Entry> = (0..MANY)
        .map(|n| Entry {
            mode: b"100644".to_vec(),
            name: format!("f{n}").into_bytes(),
            id: objects.make_blob(format!("{n}\n").as_bytes()),
        })
        .collect();
    let tree = objects.make_tree(entries);
    let by = b"t <t@example.com> 1 +0000";
    let commit = objects.make_commit(&tree, &[], by, by, b"", b"many\n");
    objects.flush().expect("flush");

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(dir.as_str())
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    git(&["update-ref", "refs/heads/main", &commit]);
    git(&["fsck", "--strict", "--no-dangling"]);
    assert_eq!(
        git(&["rev-list", "--objects", "main"]).lines().count(),
        MANY + 2
    );
}
