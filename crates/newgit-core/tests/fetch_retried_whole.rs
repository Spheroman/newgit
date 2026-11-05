//! A fetch the remote drops once is tried whole again before it goes in
//! steps. In the remote's container, a whole fetch of Spheroman/t3code once
//! failed with `HTTP 500 … expected 'packfile'`, and stepping from there took
//! over half an hour where one whole fetch takes five minutes (#144).
//!
//! The local remote here fails its first pack, then serves every one after.
//! Its upload-pack is wrapped to keep what the client asked for, so the test
//! can tell a whole fetch from a `--depth` band: only a band says `deepen`.
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
    let dir = std::env::temp_dir().join(format!("newgit-fetch-whole-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
    (TempDir(path.clone()), path)
}

fn script(path: &Utf8Path, text: &str) {
    std::fs::write(path, text).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

#[test]
fn a_dropped_fetch_is_tried_whole_again_before_it_steps() {
    let (_guard, temp) = tempdir();
    let upstream = temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "main"]);
    for n in 0..5 {
        std::fs::write(upstream.join("file.txt"), format!("{n}\n")).expect("write");
        git(&upstream, &["add", "-A"]);
        git(&upstream, &["commit", "-q", "-m", &format!("commit {n}")]);
    }
    let tip = git(&upstream, &["rev-parse", "HEAD"]);

    // The first pack fails, as Artifacts' did; every later one is served.
    let count = temp.join("packs");
    let hook = temp.join("pack-hook.sh");
    script(
        &hook,
        &format!(
            "#!/bin/sh\n\
             n=$(cat '{count}' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{count}'\n\
             if [ \"$n\" -le 1 ]; then exit 1; fi\n\
             exec \"$@\"\n"
        ),
    );
    let requests = temp.join("requests");
    let upload_pack = temp.join("upload-pack.sh");
    script(
        &upload_pack,
        &format!(
            "#!/bin/sh\n\
             tee -a '{requests}' | git -c uploadpack.packObjectsHook='{hook}' upload-pack \"$@\"\n"
        ),
    );

    let store = temp.join("store.git");
    git(&temp, &["init", "-q", "--bare", store.as_str()]);
    git(
        &store,
        &["config", "remote.up.url", &format!("file://{upstream}")],
    );
    git(
        &store,
        &["config", "remote.up.uploadpack", upload_pack.as_str()],
    );
    let git_dir = format!("--git-dir={store}");

    GitSource::fetch_refs(
        &[git_dir.as_str()],
        "up",
        &["+refs/heads/*:refs/up/*".to_owned()],
        None,
    )
    .expect("fetched on the second try");

    assert_eq!(git(&store, &["rev-parse", "refs/up/main"]), tip);
    assert_eq!(
        std::fs::read_to_string(&count)
            .expect("packs counted")
            .trim(),
        "2",
        "one dropped pack, then one whole one"
    );
    let asked =
        String::from_utf8_lossy(&std::fs::read(&requests).expect("requests kept")).into_owned();
    assert!(
        !asked.contains("deepen"),
        "the retry is a whole fetch, not a band: {asked}"
    );
    assert_eq!(
        git(&store, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
}
