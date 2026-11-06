//! Two stepped fetches into one store. Each deepens it, and Git fails the
//! second with `shallow file has changed since we read it`, which stopped
//! the copy of Spheroman/t3code into an instance's fork (#144). A local
//! remote serves packs through `uploadpack.packObjectsHook`, as in
//! `fetch_in_steps.rs`, failing any pack over 40 KiB.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::source::GitSource;

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
    let dir = std::env::temp_dir().join(format!("newgit-fetch-together-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
    (TempDir(path.clone()), path)
}

/// Sixty commits rewriting one file with incompressible bytes: five times
/// what the remote below sends at once.
fn upstream(temp: &Utf8Path, name: &str, seed: u64) -> (Utf8PathBuf, String) {
    let repo = temp.join(name);
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    let mut state = seed;
    for n in 0..60 {
        let bytes: Vec<u8> = (0..3500)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        std::fs::write(repo.join("file.bin"), bytes).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("commit {n}")]);
    }
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    (repo, tip)
}

fn limiting_hook(temp: &Utf8Path) -> Utf8PathBuf {
    let hook = temp.join("pack-hook.sh");
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         out=$(mktemp)\n\
         \"$@\" > \"$out\" || exit 1\n\
         size=$(wc -c < \"$out\")\n\
         if [ \"$size\" -gt 40960 ]; then rm -f \"$out\"; exit 1; fi\n\
         cat \"$out\"; rm -f \"$out\"\n",
    )
    .expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    hook
}

#[test]
fn two_stepped_fetches_into_one_store_both_finish() {
    let (_guard, temp) = tempdir();
    let (one, one_tip) = upstream(&temp, "one", 7);
    let (two, two_tip) = upstream(&temp, "two", 11);
    let hook = limiting_hook(&temp);
    let store = temp.join("store.git");
    git(&temp, &["init", "-q", "--bare", store.as_str()]);
    for (name, url) in [("one", &one), ("two", &two)] {
        git(
            &store,
            &[
                "config",
                &format!("remote.{name}.url"),
                &format!("file://{url}"),
            ],
        );
        git(
            &store,
            &[
                "config",
                &format!("remote.{name}.uploadpack"),
                &format!("git -c uploadpack.packObjectsHook={hook} upload-pack"),
            ],
        );
    }
    let git_dir = format!("--git-dir={store}");

    let results = std::thread::scope(|scope| {
        let fetches = ["one", "two"].map(|name| {
            let git_dir = &git_dir;
            scope.spawn(move || {
                GitSource::fetch_refs(
                    &[git_dir.as_str()],
                    name,
                    &[format!("+refs/heads/*:refs/{name}/*")],
                    None,
                )
            })
        });
        fetches.map(|fetch| fetch.join().expect("the fetch ran"))
    });
    for result in results {
        result.expect("fetched");
    }

    assert_eq!(git(&store, &["rev-parse", "refs/one/main"]), one_tip);
    assert_eq!(git(&store, &["rev-parse", "refs/two/main"]), two_tip);
    assert_eq!(
        git(&store, &["rev-parse", "--is-shallow-repository"]),
        "false",
        "both histories whole"
    );
    assert_eq!(git(&store, &["rev-list", "--count", "refs/one/main"]), "60");
    assert_eq!(git(&store, &["rev-list", "--count", "refs/two/main"]), "60");
}
