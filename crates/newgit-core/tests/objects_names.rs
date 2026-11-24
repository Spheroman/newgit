//! A name Git does not find is `None`, whatever it holds (#460 review):
//! `git cat-file` echoes a missing name as asked, so one with spaces in it
//! must not read as an object.

use std::process::Command;

use camino::Utf8PathBuf;
use newgit_core::objects::Objects;

#[test]
fn a_missing_name_with_spaces_is_not_an_object() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(temp.path().join("a.git")).expect("utf-8");
    let init = Command::new("git")
        .args(["init", "-q", "--bare", dir.as_str()])
        .status()
        .expect("git runs");
    assert!(init.success());
    let mut objects = Objects::open(&dir).expect("open");
    let blob = objects.make_blob(b"x\n");
    objects.flush().expect("flush");
    for name in ["no such", "refs/heads/a b c", "a blob 12"] {
        assert_eq!(objects.resolve(name).expect("asks"), None, "{name:?}");
        assert!(!objects.contains(name).expect("asks"), "{name:?}");
        assert_eq!(objects.raw(name).expect("asks"), None, "{name:?}");
    }
    // The readers still answer after them.
    assert_eq!(
        objects.resolve(&blob).expect("asks"),
        Some((blob.clone(), "blob".to_owned()))
    );
    assert_eq!(objects.blob(&blob).expect("reads"), b"x\n");
}
