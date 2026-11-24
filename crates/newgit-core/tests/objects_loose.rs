//! Objects written in process (#445): loose objects in Git's own format,
//! which Git reads and `fsck` passes, and a commit's missing objects copied
//! from one repository into another, stopping at what it has.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::objects::{COPY_LIMIT, Entry, Objects};

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn bare(temp: &Utf8Path, name: &str) -> Utf8PathBuf {
    let dir = temp.join(name);
    git(temp, &["init", "-q", "--bare", dir.as_str()]);
    dir
}

fn blob(dir: &Utf8Path, text: &str) -> String {
    let path = dir.join("blob.tmp");
    std::fs::write(&path, text).expect("write");
    let id = git(dir, &["hash-object", "-w", path.as_str()]);
    std::fs::remove_file(path).expect("remove");
    id
}

fn temp() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
    (dir, path)
}

const BY: &[u8] = b"t <t@example.com> 1 +0000";

/// A commit of one file, `name`, holding `text`, on `parents`.
fn commit(
    objects: &mut Objects,
    dir: &Utf8Path,
    name: &str,
    text: &str,
    parents: &[String],
) -> String {
    let file = blob(dir, text);
    let tree = objects.make_tree(vec![Entry {
        mode: b"100644".to_vec(),
        name: name.as_bytes().to_vec(),
        id: file,
    }]);
    let dir_tree = objects.make_tree(vec![Entry {
        mode: b"40000".to_vec(),
        name: b"docs".to_vec(),
        id: tree,
    }]);
    objects.make_commit(
        &dir_tree,
        parents,
        BY,
        BY,
        b"",
        format!("{name}\n").as_bytes(),
    )
}

#[test]
fn objects_flushed_are_gits_own() {
    let (_guard, temp) = temp();
    let dir = bare(&temp, "a.git");
    let mut objects = Objects::open(&dir).expect("open");
    let first = commit(&mut objects, &dir, "a.md", "one\n", &[]);
    let second = commit(
        &mut objects,
        &dir,
        "b.md",
        "two\n",
        std::slice::from_ref(&first),
    );
    objects.flush().expect("flush");
    assert_eq!(git(&dir, &["cat-file", "-t", &second]), "commit");
    assert_eq!(git(&dir, &["rev-parse", &format!("{second}~1")]), first);
    assert_eq!(git(&dir, &["show", &format!("{second}:docs/b.md")]), "two");
    git(&dir, &["update-ref", "refs/heads/main", &second]);
    git(&dir, &["fsck", "--strict", "--no-dangling"]);
    // Flushing again writes nothing new and changes nothing.
    let tree = objects.commit(&second).expect("read").tree;
    objects.make_commit(&tree, &[], BY, BY, b"", b"x\n");
    objects.flush().expect("flush again");
    git(&dir, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn a_repository_with_a_work_tree_keeps_them_in_its_git_dir() {
    let (_guard, temp) = temp();
    let dir = temp.join("work");
    git(&temp, &["init", "-q", dir.as_str()]);
    let mut objects = Objects::open(&dir).expect("open");
    let made = commit(&mut objects, &dir, "a.md", "one\n", &[]);
    objects.flush().expect("flush");
    assert!(
        dir.join(".git/objects")
            .join(&made[..2])
            .join(&made[2..])
            .is_file()
    );
    assert_eq!(git(&dir, &["cat-file", "-t", &made]), "commit");
}

#[test]
fn a_copy_takes_only_what_the_other_repository_lacks() {
    let (_guard, temp) = temp();
    let from_dir = bare(&temp, "from.git");
    let into_dir = bare(&temp, "into.git");
    let mut from = Objects::open(&from_dir).expect("open");
    let first = commit(&mut from, &from_dir, "a.md", "one\n", &[]);
    from.flush().expect("flush");
    git(&from_dir, &["update-ref", "refs/heads/main", &first]);
    // `into` has the first commit already, as a store has its history.
    git(
        &into_dir,
        &[
            "fetch",
            "-q",
            from_dir.as_str(),
            "refs/heads/main:refs/heads/main",
        ],
    );
    let second = commit(
        &mut from,
        &from_dir,
        "b.md",
        "two\n",
        std::slice::from_ref(&first),
    );
    from.flush().expect("flush");

    let mut into = Objects::open(&into_dir).expect("open");
    into.set_durable(true);
    // The commit, its tree, `docs`, and the new blob: nothing of the first.
    assert_eq!(
        into.copy_reachable(&mut from, &second).expect("copy"),
        Some(4)
    );
    git(&into_dir, &["update-ref", "refs/heads/main", &second]);
    git(&into_dir, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(
        git(&into_dir, &["show", &format!("{second}:docs/b.md")]),
        "two"
    );
    // Copied again, nothing is missing.
    assert_eq!(
        into.copy_reachable(&mut from, &second).expect("copy"),
        Some(0)
    );
}

#[test]
fn a_copy_too_large_is_left_to_git_and_writes_nothing() {
    let (_guard, temp) = temp();
    let from_dir = bare(&temp, "from.git");
    let into_dir = bare(&temp, "into.git");
    let mut from = Objects::open(&from_dir).expect("open");
    let entries: Vec<Entry> = (0..=COPY_LIMIT.0)
        .map(|n| Entry {
            mode: b"100644".to_vec(),
            name: format!("f{n}").into_bytes(),
            id: blob(&from_dir, &format!("{n}\n")),
        })
        .collect();
    let tree = from.make_tree(entries);
    let made = from.make_commit(&tree, &[], BY, BY, b"", b"many\n");
    from.flush().expect("flush");
    let mut into = Objects::open(&into_dir).expect("open");
    assert_eq!(into.copy_reachable(&mut from, &made).expect("copy"), None);
    assert!(!into.contains(&made).expect("asks"));
    assert_eq!(git(&into_dir, &["count-objects"]), "0 objects, 0 kilobytes");
}

#[test]
fn resolve_reads_a_ref_as_it_is_now() {
    let (_guard, temp) = temp();
    let dir = bare(&temp, "a.git");
    let mut objects = Objects::open(&dir).expect("open");
    let first = commit(&mut objects, &dir, "a.md", "one\n", &[]);
    let second = commit(&mut objects, &dir, "b.md", "two\n", &[]);
    objects.flush().expect("flush");
    assert_eq!(objects.resolve("refs/heads/main").expect("asks"), None);
    git(&dir, &["update-ref", "refs/heads/main", &first]);
    assert_eq!(
        objects.resolve("refs/heads/main").expect("asks"),
        Some((first, "commit".to_owned()))
    );
    git(&dir, &["update-ref", "refs/heads/main", &second]);
    assert_eq!(
        objects
            .resolve("refs/heads/main")
            .expect("asks")
            .map(|(id, _)| id),
        Some(second)
    );
}
