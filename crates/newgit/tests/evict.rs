//! `newgit stop --evict` and the `newgit start` after it: a settled T3 Code
//! thread's workspace is checkpointed and deleted, and comes back from that
//! checkpoint, at the same path, when the thread is picked up again.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::Value;

fn git_out(dir: &Utf8Path, args: &[&str]) -> String {
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

fn git(dir: &Utf8Path, args: &[&str]) {
    git_out(dir, args);
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
    let dir = std::env::temp_dir().join(format!("newgit-evict-{}-{n}", std::process::id()));
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

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    Run {
        status: output.status,
        stdout: String::from_utf8(output.stdout).expect("utf8 stdout"),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn newgit_ok(repo: &Utf8Path, args: &[&str]) -> String {
    let run = newgit(repo, args);
    assert!(
        run.status.success(),
        "newgit {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    run.stdout
}

fn json(repo: &Utf8Path, args: &[&str]) -> Value {
    let stdout = newgit_ok(repo, args);
    serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("`newgit {args:?}` is not one JSON document ({error}):\n{stdout}")
    })
}

fn running(store: &MetadataStore, resource: &str) -> bool {
    let pid_file = store
        .paths()
        .state
        .join("x")
        .join(format!("{resource}.pid"));
    let Some(pid) = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
    else {
        return false;
    };
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

/// Every path under `dir`, relative, sorted.
fn tree(dir: &Utf8Path) -> Vec<String> {
    fn walk(root: &Utf8Path, dir: &Utf8Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = Utf8PathBuf::from_path_buf(entry.expect("entry").path()).expect("utf8");
            out.push(path.strip_prefix(root).expect("under root").to_string());
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn evict_then_start_brings_the_workspace_back_as_it_was() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let workspace = temp.join("workspaces/x");
    newgit_ok(&repo, &["action", "db.start", "x"]);

    // Committed, uncommitted, untracked, and ignored work.
    std::fs::write(workspace.join("committed.txt"), "c\n").expect("write");
    git(&workspace, &["add", "committed.txt"]);
    git(&workspace, &["commit", "-q", "-m", "work"]);
    let head = git_out(&workspace, &["rev-parse", "HEAD"]);
    std::fs::write(workspace.join("README.md"), "edited\n").expect("write");
    std::fs::write(workspace.join("untracked.txt"), "u\n").expect("write");
    std::fs::create_dir_all(workspace.join("target")).expect("mkdir");
    std::fs::write(workspace.join("target/build.o"), "o\n").expect("write");

    // `target/` would not come back, so a plain evict refuses, names it,
    // and deletes nothing.
    let refused = newgit(&repo, &["stop", "x", "--evict"]);
    assert!(!refused.status.success());
    assert!(refused.stderr.contains("`target`"), "{}", refused.stderr);
    assert!(
        refused.stderr.contains("--discard-ignored"),
        "{}",
        refused.stderr
    );
    assert!(workspace.join("target/build.o").is_file());
    assert!(running(&store, "db"), "a refusal stops nothing");

    let evicted = json(
        &repo,
        &["stop", "x", "--evict", "--discard-ignored", "--json"],
    );
    assert_eq!(
        evicted["resources"],
        serde_json::json!([{"name": "db", "action": "start", "status": "stopped"}]),
        "{evicted}"
    );
    let checkpoint = evicted["evicted"]["checkpoint"]
        .as_str()
        .unwrap_or_else(|| panic!("no checkpoint: {evicted}"))
        .to_owned();
    assert_eq!(
        evicted["evicted"]["discarded"],
        serde_json::json!(["target"])
    );
    assert!(!running(&store, "db"));
    // Only the marker stays, so commands run there still find the instance.
    assert_eq!(
        tree(&workspace),
        [
            ".newgit",
            ".newgit/local",
            ".newgit/local/.gitignore",
            ".newgit/local/instance.toml"
        ]
    );
    assert!(newgit_ok(&repo, &["status"]).contains("evicted"));

    // Nothing that needs the workspace runs, and it says how to get it back.
    let refused = newgit(&repo, &["checkpoint", "x"]);
    assert!(!refused.status.success());
    assert!(
        refused.stderr.contains("newgit start x"),
        "{}",
        refused.stderr
    );

    // An evicted instance is not one whose workspace is gone.
    newgit_ok(&repo, &["cleanup"]);
    assert!(store.paths().branches.join("x.toml").is_file());

    // A second evict has nothing left to do.
    let again = json(&repo, &["stop", "x", "--evict", "--json"]);
    assert!(again.get("evicted").is_none(), "{again}");

    let started = json(&repo, &["start", "x", "--json"]);
    assert_eq!(
        started["restored"]["checkpoint"],
        checkpoint.as_str(),
        "{started}"
    );
    assert_eq!(
        started["resources"],
        serde_json::json!([{"name": "db", "action": "start", "status": "started"}])
    );
    assert!(running(&store, "db"));

    assert_eq!(git_out(&workspace, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_out(&workspace, &["branch", "--show-current"]).trim(),
        "x"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).expect("read"),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("untracked.txt")).expect("read"),
        "u\n"
    );
    // Still uncommitted, as it was.
    let status = git_out(&workspace, &["status", "--porcelain"]);
    assert!(status.contains(" M README.md"), "{status}");
    assert!(status.contains("?? untracked.txt"), "{status}");
    assert!(
        !workspace.join("target").exists(),
        "ignored work is not kept"
    );
    assert!(!newgit_ok(&repo, &["status"]).contains("evicted"));

    // A start after that is an ordinary one.
    let again = json(&repo, &["start", "x", "--json"]);
    assert!(again.get("restored").is_none(), "{again}");
}

#[test]
fn start_refuses_to_restore_over_something_written_there_since() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let workspace = temp.join("workspaces/x");
    newgit_ok(&repo, &["stop", "x", "--evict"]);

    std::fs::write(workspace.join("notes.txt"), "mine\n").expect("write");
    let refused = newgit(&repo, &["start", "x"]);
    assert!(!refused.status.success());
    assert!(
        refused.stderr.contains("holds more than its marker"),
        "{}",
        refused.stderr
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes.txt")).expect("read"),
        "mine\n"
    );

    std::fs::remove_file(workspace.join("notes.txt")).expect("rm");
    newgit_ok(&repo, &["start", "x"]);
    assert!(workspace.join("README.md").is_file());
}

#[test]
fn start_restores_even_when_the_marker_went_too() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let workspace = temp.join("workspaces/x");
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    std::fs::remove_dir_all(&workspace).expect("rm");

    // Still an evicted instance, not a stale one to finalize.
    newgit_ok(&repo, &["cleanup"]);
    newgit_ok(&repo, &["start", "x"]);
    assert!(workspace.join("README.md").is_file());
}

#[test]
fn an_evicted_instance_can_be_removed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    newgit_ok(&repo, &["remove", "x"]);
    assert!(!temp.join("workspaces/x").exists());
    assert!(!store.paths().branches.join("x.toml").exists());
}

/// What a resource builds from the workspace — `node_modules` from a
/// lockfile — is not in the checkpoint, and comes back all the same.
#[test]
fn a_built_tree_comes_back_with_the_workspace() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("deps.toml"),
        r#"ownership = "workspace"

[identity]
paths = ["README.md"]
produces = ["node_modules"]

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/tree.txt"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#,
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let workspace = temp.join("workspaces/x");
    assert!(workspace.join("node_modules/tree.txt").is_file());

    let evicted = json(&repo, &["stop", "x", "--evict", "--json"]);
    assert_eq!(
        evicted["evicted"]["discarded"],
        serde_json::json!([]),
        "a tree a resource rebuilds is not lost: {evicted}"
    );
    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(
        std::fs::read_to_string(workspace.join("node_modules/tree.txt")).expect("read"),
        "built\n"
    );
}
