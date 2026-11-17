//! A retry newgit decides on from Git's message still runs when the
//! developer's locale is not English.
//!
//! `local_refs::update_refs` waits out another Git holding a ref's lock: Git
//! says `cannot lock ref`, and newgit tries again. Git translates that
//! message, so under a German locale it used to fail at once. Every Git
//! newgit runs now runs in the C locale (`newgit_core::git::command()`).
//!
//! The second test runs the first again, as a child of this test binary,
//! under a locale whose Git messages really are translated here. A machine
//! whose Git has no translations installed has no such locale; the test says
//! so and passes without proving anything.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::process::Command;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::local_refs;

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
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "newgit-git-locale-retry-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

/// A repository with one commit, refs in files so a lock is a `.lock` file.
fn repo(temp: &Utf8Path) -> (Utf8PathBuf, String) {
    let repo = temp.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main", "--ref-format=files"]);
    std::fs::write(repo.join("a.txt"), "a\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "a"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    (repo, head)
}

/// Developers' locales whose Git messages may be translated: each whole
/// (`LC_ALL`), and English with `LANGUAGE` naming another, as a machine with
/// only `en_US.UTF-8` installed honours.
const FOREIGN: &[&[(&str, &str)]] = &[
    &[("LC_ALL", "de_DE.UTF-8"), ("LANGUAGE", "de")],
    &[("LC_ALL", "en_US.UTF-8"), ("LANGUAGE", "de")],
    &[("LC_ALL", "C.UTF-8"), ("LANGUAGE", "de")],
    &[("LC_ALL", "fr_FR.UTF-8"), ("LANGUAGE", "fr")],
    &[("LC_ALL", "ja_JP.UTF-8"), ("LANGUAGE", "ja")],
];

/// The first of [`FOREIGN`] under which plain `git`, finding a ref's lock
/// taken, does not say `cannot lock ref`.
fn translated_locale(temp: &Utf8Path) -> Option<&'static [(&'static str, &'static str)]> {
    let (repo, head) = repo(temp);
    let lock = repo.join(".git/refs/heads/held.lock");
    std::fs::write(&lock, "").expect("lock");
    FOREIGN.iter().copied().find(|locale| {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo.as_str())
            .args(["update-ref", "refs/heads/held", &head])
            .envs(locale.iter().copied())
            .output()
            .expect("git runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        !output.status.success() && !stderr.is_empty() && !stderr.contains("cannot lock ref")
    })
}

#[test]
fn a_held_ref_lock_is_waited_out() {
    let (_guard, temp) = tempdir();
    let (repo, head) = repo(&temp);
    let lock = repo.join(".git/refs/heads/held.lock");
    std::fs::write(&lock, "").expect("lock");
    let release = {
        let lock = lock.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            std::fs::remove_file(lock).expect("unlock");
        })
    };
    let git_dir = format!("--git-dir={repo}/.git");
    local_refs::update_refs(&[&git_dir], &[("refs/heads/held".to_owned(), head.clone())])
        .expect("the lock is waited out");
    release.join().expect("unlocked");
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/held"]), head);
}

#[test]
fn a_held_ref_lock_is_waited_out_under_a_translated_locale() {
    let (_guard, temp) = tempdir();
    let Some(locale) = translated_locale(&temp) else {
        eprintln!(
            "SKIPPED: no locale here translates Git's messages (tried {FOREIGN:?}), so this \
             proves nothing on this machine"
        );
        return;
    };
    eprintln!("running under {locale:?}, whose Git messages are translated");
    let output = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "a_held_ref_lock_is_waited_out", "--nocapture"])
        .envs(locale.iter().copied())
        .output()
        .expect("the test binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "under {locale:?}:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
