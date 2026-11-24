//! A `[cleanup]` hook of an instance whose workspace is already gone runs
//! from an empty directory, never from the project root: a hook written for
//! the workspace (`rm -rf postgres-data`) must not delete the project's own
//! directory of that name.

#![allow(dead_code)]

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::Value;

fn git(dir: &Utf8Path, args: &[&str]) {
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
}

/// A directory under the OS temp dir, unique per call. On drop it stops
/// whatever the instance still runs, then removes the directory.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(self.0.join("store").as_str())
            .args(["stop", "x"])
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-cleanupcwd-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn json(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, Value) {
    let (code, stdout, stderr) = newgit(repo, args);
    let value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("`newgit {args:?}` stdout is not one JSON document ({error}):\n{stdout}\n{stderr}")
    });
    (code, value)
}

/// Where newgit's port search starts: a port the OS just handed out, so
/// two runs at once do not search the same range (#276).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("an OS-assigned port")
        .port()
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

#[test]
fn cleanup_without_a_workspace_does_not_run_in_the_project_root() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    write_resource(
        &store,
        "db",
        "ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"mkdir -p data-dir\"\n\n\
         [cleanup]\ncommand = \"rm -rf data-dir; pwd > \\\"$WHERE\\\"\"\n",
    );
    // The developer's own, in the project root, as a workspace has one.
    std::fs::create_dir_all(repo.join("data-dir")).expect("mkdir");
    std::fs::write(repo.join("data-dir/mine"), "keep\n").expect("write");

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{spawned}");
    let workspace = Utf8PathBuf::from(spawned["workspace"].as_str().expect("workspace"));
    std::fs::remove_dir_all(&workspace).expect("delete the workspace");

    let where_it_ran = temp.join("where");
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(["remove", "x", "--force"])
        .env("WHERE", where_it_ran.as_str())
        .output()
        .expect("newgit runs");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        repo.join("data-dir/mine").exists(),
        "cleanup deleted the project root's own directory: {said}"
    );
    let ran_in = std::fs::read_to_string(&where_it_ran)
        .unwrap_or_else(|_| panic!("the cleanup hook did not run: {said}"));
    assert_ne!(
        Utf8Path::new(ran_in.trim()).canonicalize_utf8().ok(),
        repo.canonicalize_utf8().ok(),
        "{said}"
    );
}
