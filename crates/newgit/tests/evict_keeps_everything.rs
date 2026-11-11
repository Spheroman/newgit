//! `newgit stop --evict` then `newgit start` gives back exactly what was
//! there, or the eviction refuses and deletes nothing: what lives only in
//! `.git/`, the branch the workspace is on, what no checkpoint carries, the
//! commands run while evicted, and an eviction or restore cut short.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

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
    let dir = std::env::temp_dir().join(format!("newgit-evict-keep-{}-{n}", std::process::id()));
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

fn spawn_with_work(temp: &Utf8Path) -> (MetadataStore, Utf8PathBuf, Utf8PathBuf) {
    let store = setup(temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    (store, repo, temp.join("workspaces/x"))
}

fn read(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read {path}"))
}

/// What lives only in the workspace's `.git/` — what is staged apart from
/// the working tree, a branch nobody pushed, a tag, the stash — comes back
/// as it was, along with the branch checked out.
#[test]
fn git_state_only_the_workspace_had_comes_back() {
    let (_guard, temp) = tempdir();
    let (_store, repo, ws) = spawn_with_work(&temp);

    // A side branch with a commit of its own, tagged, never pushed.
    git(&ws, &["checkout", "-q", "-b", "side"]);
    std::fs::write(ws.join("side.txt"), "side\n").expect("write");
    git(&ws, &["add", "side.txt"]);
    git(&ws, &["commit", "-q", "-m", "side work"]);
    git(&ws, &["tag", "side-tag"]);
    let side = git_out(&ws, &["rev-parse", "side"]);
    git(&ws, &["checkout", "-q", "x"]);

    // Two stash entries.
    std::fs::write(ws.join("README.md"), "stash one\n").expect("write");
    git(&ws, &["stash", "push", "-q", "-m", "first"]);
    std::fs::write(ws.join("README.md"), "stash two\n").expect("write");
    git(&ws, &["stash", "push", "-q", "-m", "second"]);
    let stash = git_out(&ws, &["stash", "list", "--format=%H %gs"]);

    // Staged one way, edited another; a new file staged; one untracked.
    std::fs::write(ws.join("README.md"), "staged\n").expect("write");
    git(&ws, &["add", "README.md"]);
    std::fs::write(ws.join("README.md"), "working\n").expect("write");
    std::fs::write(ws.join("new.txt"), "new\n").expect("write");
    git(&ws, &["add", "new.txt"]);
    std::fs::write(ws.join("untracked.txt"), "u\n").expect("write");
    let status = git_out(&ws, &["status", "--porcelain"]);
    let staged_readme = git_out(&ws, &["show", ":README.md"]);

    newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(!ws.join(".git").exists());
    newgit_ok(&repo, &["start", "x"]);

    assert_eq!(git_out(&ws, &["status", "--porcelain"]), status);
    assert_eq!(git_out(&ws, &["show", ":README.md"]), staged_readme);
    assert_eq!(read(&ws.join("README.md")), "working\n");
    assert_eq!(git_out(&ws, &["rev-parse", "side"]), side);
    assert_eq!(git_out(&ws, &["rev-parse", "side-tag"]), side);
    assert_eq!(git_out(&ws, &["stash", "list", "--format=%H %gs"]), stash);
    assert_eq!(git_out(&ws, &["branch", "--show-current"]).trim(), "x");
    // Nothing of the eviction is left in the store.
    let left = git_out(&repo, &["for-each-ref", "refs/newgit/evicted/"]);
    assert!(left.trim().is_empty(), "{left}");
}

/// A workspace on a branch an agent made, or with a detached HEAD, comes
/// back on it.
#[test]
fn another_branch_or_a_detached_head_comes_back() {
    let (_guard, temp) = tempdir();
    let (_store, repo, ws) = spawn_with_work(&temp);
    git(&ws, &["checkout", "-q", "-b", "try-x"]);
    std::fs::write(ws.join("try.txt"), "t\n").expect("write");
    git(&ws, &["add", "try.txt"]);
    git(&ws, &["commit", "-q", "-m", "try"]);
    let tip = git_out(&ws, &["rev-parse", "HEAD"]);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(git_out(&ws, &["branch", "--show-current"]).trim(), "try-x");
    assert_eq!(git_out(&ws, &["rev-parse", "HEAD"]), tip);

    git(&ws, &["checkout", "-q", "--detach", "HEAD~1"]);
    let detached = git_out(&ws, &["rev-parse", "HEAD"]);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(git_out(&ws, &["branch", "--show-current"]).trim(), "");
    assert_eq!(git_out(&ws, &["rev-parse", "HEAD"]), detached);
    assert_eq!(git_out(&ws, &["rev-parse", "try-x"]), tip);
}

fn refused(repo: &Utf8Path, args: &[&str], says: &str) {
    let run = newgit(repo, args);
    assert!(
        !run.status.success(),
        "newgit {args:?} should refuse:\n{}",
        run.stdout
    );
    assert!(
        run.stderr.contains(says),
        "wanted {says:?} in:\n{}",
        run.stderr
    );
}

/// What no checkpoint carries stops the eviction before anything is
/// deleted: an ignored secret, a merge in the middle, an untracked
/// repository, and a process still working in the directory.
#[test]
fn evict_refuses_and_deletes_nothing_when_something_would_not_come_back() {
    let (_guard, temp) = tempdir();
    let (store, repo, ws) = spawn_with_work(&temp);
    newgit_ok(&repo, &["action", "db.start", "x"]);

    // Ignored, and in no tracker: refused even with nothing else in the way.
    std::fs::write(ws.join(".gitignore"), "target/\n.env.local\n").expect("write");
    git(&ws, &["commit", "-q", "-am", "ignore env"]);
    std::fs::write(ws.join(".env.local"), "SECRET=1\n").expect("write");
    refused(&repo, &["stop", "x", "--evict"], "`.env.local`");
    assert!(running(&store, "db"), "refused before stopping anything");
    std::fs::remove_file(ws.join(".env.local")).expect("rm");

    // A merge with a conflict in the middle.
    git(&ws, &["checkout", "-q", "-b", "other"]);
    std::fs::write(ws.join("README.md"), "other\n").expect("write");
    git(&ws, &["commit", "-q", "-am", "other"]);
    git(&ws, &["checkout", "-q", "x"]);
    std::fs::write(ws.join("README.md"), "mine\n").expect("write");
    git(&ws, &["commit", "-q", "-am", "mine"]);
    let merge = Command::new("git")
        .args(["-C", ws.as_str(), "merge", "-q", "other"])
        .output()
        .expect("git runs");
    assert!(!merge.status.success(), "the merge conflicts");
    refused(&repo, &["stop", "x", "--evict"], "in the middle of a merge");
    git(&ws, &["merge", "--abort"]);

    // An untracked repository of its own.
    std::fs::create_dir_all(ws.join("vendor/lib")).expect("mkdir");
    git(&ws.join("vendor/lib"), &["init", "-q"]);
    std::fs::write(ws.join("vendor/lib/a.txt"), "a\n").expect("write");
    refused(&repo, &["stop", "x", "--evict"], "`vendor/lib`");
    std::fs::remove_dir_all(ws.join("vendor")).expect("rm");

    // A process sitting in the workspace.
    let mut shell = Command::new("sleep")
        .arg("1000")
        .current_dir(ws.as_str())
        .spawn()
        .expect("sleep runs");
    refused(&repo, &["stop", "x", "--evict"], &shell.id().to_string());
    let _ = shell.kill();
    let _ = shell.wait();

    assert!(ws.join(".git").is_dir(), "nothing was deleted");
    assert_eq!(read(&ws.join("README.md")), "mine\n");
    let started = newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(started.contains("Evicted"), "{started}");
}

/// A hand edit to a rendered file is the one thing a checkpoint does not
/// carry by design, so it refuses an eviction too.
#[test]
fn evict_refuses_an_edit_to_a_rendered_file() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::write(repo.join("app.toml"), "port = 1\n").expect("write");
    git(&repo, &["add", "app.toml"]);
    git(&repo, &["commit", "-q", "-m", "app"]);
    std::fs::write(
        store.paths().resources.join("cfg.toml"),
        "ownership = \"branch\"\n\n[[render]]\npath = \"app.toml\"\nreplace = [\n  { find = \"port = 1\", with = \"port = 2\" },\n]\n",
    )
    .expect("write resource");
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    assert_eq!(read(&ws.join("app.toml")), "port = 2\n");
    std::fs::write(ws.join("app.toml"), "port = 2\nedited = true\n").expect("write");
    refused(&repo, &["stop", "x", "--evict"], "`app.toml` has changes");
    assert_eq!(read(&ws.join("app.toml")), "port = 2\nedited = true\n");

    // Untouched, the render comes back rendered.
    std::fs::write(ws.join("app.toml"), "port = 2\n").expect("write");
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(read(&ws.join("app.toml")), "port = 2\n");
}

/// Every command that needs the workspace says it was evicted and how to
/// bring it back; `status` says `evicted`.
#[test]
fn commands_on_an_evicted_instance_say_so() {
    let (_guard, temp) = tempdir();
    let (_store, repo, ws) = spawn_with_work(&temp);
    newgit_ok(&repo, &["checkpoint", "x"]);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let status = newgit_ok(&repo, &["status"]);
    assert!(status.contains("evicted"), "{status}");
    for args in [
        &["checkpoint", "x"][..],
        &["undo", "x"],
        &["run", "x", "--", "true"],
        &["merge", "x"],
    ] {
        refused(&repo, args, "newgit start x");
    }
    // From inside the path too, where only the marker is.
    refused(&ws, &["checkpoint"], "newgit start x");
}

/// A restore that stopped part way is run again by the next `start`, over
/// what it left; and an eviction that stopped between moving the workspace
/// aside and deleting it leaves nothing `start` and `cleanup` cannot finish.
#[test]
fn an_interrupted_restore_or_eviction_finishes_on_the_next_command() {
    let (_guard, temp) = tempdir();
    let (store, repo, ws) = spawn_with_work(&temp);
    std::fs::write(ws.join("untracked.txt"), "u\n").expect("write");
    newgit_ok(&repo, &["stop", "x", "--evict"]);

    // As a restore killed after the clone and before the undo leaves it.
    let restoring = store.paths().state.join("x/evict-restoring");
    std::fs::write(&restoring, "whatever").expect("write");
    std::fs::write(ws.join("half.txt"), "half a clone\n").expect("write");
    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(read(&ws.join("untracked.txt")), "u\n");
    assert!(!ws.join("half.txt").exists());
    assert!(!restoring.exists());
    assert!(!newgit_ok(&repo, &["status"]).contains("evicted"));

    // As an eviction killed after the move aside: the path is gone, the
    // moved workspace is beside it.
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let trash = temp.join("workspaces/.x.evicting");
    std::fs::rename(&ws, &trash).expect("rename");
    std::fs::create_dir_all(trash.join("leftover")).expect("mkdir");
    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(read(&ws.join("untracked.txt")), "u\n");
    assert!(!trash.exists(), "start deletes what the eviction left");
}

/// Removing an evicted instance deletes what the eviction kept: the Git
/// state in the store and the token in the state directory.
#[test]
fn removing_an_evicted_instance_leaves_nothing_of_the_eviction() {
    let (_guard, temp) = tempdir();
    let (store, repo, ws) = spawn_with_work(&temp);
    git(&ws, &["checkout", "-q", "-b", "side"]);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(
        !git_out(&repo, &["for-each-ref", "refs/newgit/evicted/x/"])
            .trim()
            .is_empty()
    );
    newgit_ok(&repo, &["remove", "x"]);
    assert!(
        git_out(&repo, &["for-each-ref", "refs/newgit/evicted/"])
            .trim()
            .is_empty()
    );
    assert!(!store.paths().state.join("x").exists());
}
