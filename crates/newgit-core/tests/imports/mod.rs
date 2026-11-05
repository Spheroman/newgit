//! Helpers for the tests of outside repositories as trackers (#140–#142):
//! an upstream repository standing in for Spheroman/t3code, and a project
//! that imports it. A file that wants these says `mod imports;`.

#![allow(dead_code)]

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

pub fn git_env(dir: &Utf8Path, args: &[&str], env: &[(&str, String)]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "ana")
        .env("GIT_AUTHOR_EMAIL", "ana@example.com")
        .env("GIT_COMMITTER_NAME", "ana")
        .env("GIT_COMMITTER_EMAIL", "ana@example.com")
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub fn git(dir: &Utf8Path, args: &[&str]) -> String {
    git_env(dir, args, &[])
}

/// Whether `git args` succeeds.
pub fn git_ok(dir: &Utf8Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git runs")
        .status
        .success()
}

/// Commit `files` in `dir` as `who`, dated `time` (seconds since the epoch).
pub fn commit_as(
    dir: &Utf8Path,
    who: &str,
    time: i64,
    files: &[(&str, &str)],
    message: &str,
) -> String {
    for (path, text) in files {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, text).expect("write");
    }
    git(dir, &["add", "-A"]);
    let date = format!("@{time} +0000");
    git_env(
        dir,
        &["commit", "-q", "--allow-empty", "-m", message],
        &[
            ("GIT_AUTHOR_NAME", who.to_owned()),
            ("GIT_AUTHOR_EMAIL", format!("{who}@example.com")),
            ("GIT_COMMITTER_NAME", who.to_owned()),
            ("GIT_COMMITTER_EMAIL", format!("{who}@example.com")),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_DATE", date),
        ],
    );
    git(dir, &["rev-parse", "HEAD"])
}

pub fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// The upstream: a repository of its own on branch `newgit`, its files at
/// its root, with a merge in its history. Returns its path.
pub fn upstream(temp: &Utf8Path) -> Utf8PathBuf {
    let repo = temp.join("upstream");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "newgit"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    commit_as(
        &repo,
        "theo",
        1_000_000,
        &[("package.json", "{}\n"), ("src/app.ts", "one\n")],
        "t3: first",
    );
    git(&repo, &["switch", "-q", "-c", "side"]);
    commit_as(
        &repo,
        "julius",
        1_000_100,
        &[("src/side.ts", "side\n")],
        "t3: side",
    );
    git(&repo, &["switch", "-q", "newgit"]);
    commit_as(
        &repo,
        "theo",
        1_000_200,
        &[("src/app.ts", "two\n")],
        "t3: second",
    );
    git_env(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "Merge side", "side"],
        &[
            ("GIT_AUTHOR_DATE", "@1000300 +0000".to_owned()),
            ("GIT_COMMITTER_DATE", "@1000300 +0000".to_owned()),
        ],
    );
    repo
}

/// A project at `temp/store`, initialized and committed, `main` published.
pub fn project(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);
    BranchManager::open(store.clone())
        .expect("manager")
        .merge("main", "main")
        .expect("publish");
    store
}

pub fn manager(store: &MetadataStore) -> BranchManager {
    BranchManager::open(store.clone()).expect("manager")
}

/// Import `upstream`'s `newgit` as `t3code` at `apps/t3code`.
pub fn import(
    store: &MetadataStore,
    upstream: &Utf8Path,
) -> newgit_core::manager::ImportGitTrackerOutcome {
    manager(store)
        .import_git_tracker(
            "t3code",
            "project-devs",
            Utf8Path::new("apps/t3code"),
            upstream.as_str(),
            Some("newgit"),
        )
        .expect("import")
}

/// A ref in the tracker store, if it exists.
pub fn store_ref(store: &MetadataStore, name: &str) -> Option<String> {
    let tracker_store = store.paths().tracker_store.clone();
    let output = Command::new("git")
        .arg(format!("--git-dir={tracker_store}"))
        .args(["rev-parse", "-q", "--verify", &format!("{name}^{{commit}}")])
        .output()
        .expect("git runs");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Every ref anywhere (project repo and tracker store) mentioning `needle`.
pub fn refs_mentioning(store: &MetadataStore, needle: &str) -> Vec<String> {
    let repo = store.paths().project_root.clone();
    let tracker_store = store.paths().tracker_store.clone();
    let mut refs: Vec<String> = git(&repo, &["for-each-ref", "--format=%(refname)"])
        .lines()
        .chain(
            git(
                &repo,
                &[
                    &format!("--git-dir={tracker_store}"),
                    "for-each-ref",
                    "--format=%(refname)",
                ],
            )
            .lines(),
        )
        .filter(|name| name.contains(needle))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    refs.sort();
    refs
}

/// A bare copy of `upstream`, standing in for Spheroman/t3code on GitHub.
pub fn github_copy(temp: &Utf8Path, upstream: &Utf8Path, name: &str) -> Utf8PathBuf {
    let bare = temp.join(format!("{name}.git"));
    git(
        temp,
        &["clone", "-q", "--bare", upstream.as_str(), bare.as_str()],
    );
    bare
}

/// Merge a pull request on `bare`'s `branch`: a clone, a commit of `files`
/// on a topic branch, merged with `--no-ff`, pushed. Returns the new tip.
pub fn merge_pr(
    temp: &Utf8Path,
    bare: &Utf8Path,
    branch: &str,
    files: &[(&str, &str)],
    message: &str,
) -> String {
    let clone = temp.join(format!("pr-{}", message.replace([' ', ':', '/'], "-")));
    git(
        temp,
        &["clone", "-q", "-b", branch, bare.as_str(), clone.as_str()],
    );
    git(&clone, &["config", "commit.gpgSign", "false"]);
    git(&clone, &["switch", "-q", "-c", "topic"]);
    commit_as(&clone, "pat", 2_000_000_000, files, message);
    git(&clone, &["switch", "-q", branch]);
    git(
        &clone,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #1",
            "topic",
        ],
    );
    git(&clone, &["push", "-q", "origin", branch]);
    git(&clone, &["rev-parse", "HEAD"])
}

/// Define export target `name` in the project root (uncommitted: targets are
/// read from the working tree).
pub fn define_target(store: &MetadataStore, name: &str, text: &str) {
    let dir = store.paths().project_root.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(format!("{name}.toml")), text).expect("write");
}
