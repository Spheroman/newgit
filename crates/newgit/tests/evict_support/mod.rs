//! What the eviction round-trip tests share: a project with one
//! long-running resource and a workspace root, newgit run from a directory,
//! and a snapshot of everything a workspace is, to compare before an
//! eviction and after the `start` that brings it back.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

pub fn git_out(dir: &Utf8Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).expect("utf8")
}

pub fn git(dir: &Utf8Path, args: &[&str]) {
    git_out(dir, args);
}

/// A directory under the OS temp dir, unique per call. On drop it stops
/// whatever the instance still runs, then removes the directory.
pub struct TempDir(pub Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(self.0.join("store").as_str())
            .args(["stop", "x"])
            .output();
        #[cfg(unix)]
        {
            // A test may leave a directory unwritable on purpose.
            let _ = Command::new("chmod")
                .args(["-R", "u+rwx", self.0.as_str()])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn tempdir(tag: &str) -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

pub fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join(".gitignore"), "target/\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        "ownership = \"branch\"\n\n[actions.start]\ncommand = \"sleep 1000\"\nlong_running = true\n",
    )
    .expect("write resource");
    store
}

pub struct Run {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

pub fn newgit_env(dir: &Utf8Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .envs(env.iter().copied())
        .output()
        .expect("newgit runs");
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

pub fn newgit(dir: &Utf8Path, args: &[&str]) -> Run {
    newgit_env(dir, args, &[])
}

pub fn newgit_ok(dir: &Utf8Path, args: &[&str]) -> Run {
    let run = newgit(dir, args);
    assert!(
        run.status.success(),
        "newgit {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    run
}

/// `newgit <args>` fails, saying `says`.
pub fn refused(dir: &Utf8Path, args: &[&str], says: &str) -> Run {
    let run = newgit(dir, args);
    assert!(
        !run.status.success(),
        "newgit {args:?} should have failed:\n{}",
        run.stdout
    );
    assert!(
        run.stderr.contains(says),
        "wanted {says:?} in:\n{}",
        run.stderr
    );
    run
}

pub fn chmod(path: &Utf8Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

pub fn mode(path: &Utf8Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

/// Everything a workspace is, as `name → description`: each path under it
/// but `.git/` (its kind, mode, and bytes or link target, empty
/// directories included), `git status --porcelain=v2`, every ref but
/// `origin`'s (a new clone fetches those afresh), the stash, the local
/// config the user sets, the hooks, and the notes.
pub fn snapshot(ws: &Utf8Path) -> BTreeMap<String, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut out = BTreeMap::new();
    let mut pending = vec![ws.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let path = Utf8PathBuf::from_path_buf(entry.expect("entry").path()).expect("utf8");
            let relative = path.strip_prefix(ws).expect("inside").to_string();
            if relative == ".git" {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path).expect("stat");
            let mode = metadata.permissions().mode() & 0o7777;
            let what = if metadata.file_type().is_symlink() {
                format!(
                    "link -> {}",
                    std::fs::read_link(&path).expect("link").display()
                )
            } else if metadata.is_dir() {
                pending.push(path.clone());
                format!("dir {mode:o}")
            } else {
                format!(
                    "file {mode:o} {:?}",
                    String::from_utf8_lossy(&std::fs::read(&path).expect("read"))
                )
            };
            out.insert(relative, what);
        }
    }
    out.insert(
        "@status".to_owned(),
        git_out(ws, &["status", "--porcelain=v2", "--branch", "-uall"]),
    );
    out.insert(
        "@refs".to_owned(),
        git_out(ws, &["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .filter(|line| !line.starts_with("refs/remotes/origin/"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    out.insert(
        "@stash".to_owned(),
        git_out(ws, &["stash", "list", "--format=%H %gs"]),
    );
    out.insert(
        "@config".to_owned(),
        git_out(ws, &["config", "--local", "--list"])
            .lines()
            .filter(|line| {
                line.starts_with("user.")
                    || line.starts_with("branch.")
                    || line.starts_with("remote.up.")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let hooks = ws.join(".git/hooks");
    let mut listed: Vec<String> = std::fs::read_dir(&hooks)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str().map(ToOwned::to_owned))
                .filter(|name| !name.ends_with(".sample"))
                .map(|name| {
                    let path = hooks.join(&name);
                    format!(
                        "{name} {:o} {}",
                        mode(&path),
                        std::fs::read_to_string(&path).unwrap_or_default()
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    listed.sort();
    out.insert("@hooks".to_owned(), listed.join("\n"));
    out.insert(
        "@notes".to_owned(),
        Command::new("git")
            .args(["-C", ws.as_str(), "notes", "list"])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default(),
    );
    out
}

/// `before` and `after` are the same snapshot, naming what differs.
pub fn assert_same(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) {
    let mut diff = Vec::new();
    for key in before.keys().chain(after.keys()) {
        let (a, b) = (before.get(key), after.get(key));
        if a != b && !diff.iter().any(|(k, _, _): &(String, _, _)| k == key) {
            diff.push((key.clone(), a.cloned(), b.cloned()));
        }
    }
    assert!(
        diff.is_empty(),
        "the workspace did not come back as it was:\n{}",
        diff.iter()
            .map(|(key, a, b)| format!("  {key}:\n    before: {a:?}\n    after:  {b:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Every path under `dir`, `.git/` but the workspace's own included: proof
/// that nothing was deleted.
pub fn listing(dir: &Utf8Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = Utf8PathBuf::from_path_buf(entry.path()).expect("utf8");
            if path.is_dir() && !path.is_symlink() {
                pending.push(path.clone());
            }
            out.push(path.strip_prefix(dir).expect("inside").to_string());
        }
    }
    out.sort();
    out
}
