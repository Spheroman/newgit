//! `newgit claude-code`: the install command writes Claude Code's
//! WorktreeCreate and WorktreeRemove hooks, and the hooks spawn and remove an
//! instance with nothing on stdout but the workspace path Claude Code reads.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

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

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops — the same hand-rolled guard as `spawn_exit.rs`.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-claude-code-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> Utf8PathBuf {
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
    repo
}

fn newgit(cwd: &Utf8Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(cwd.as_str())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("newgit runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("newgit exits")
}

#[test]
fn install_writes_both_hooks_once() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);

    let first = newgit(&repo, &["claude-code", "install"], "");
    assert!(first.status.success(), "{first:?}");
    let second = newgit(&repo, &["claude-code", "install"], "");
    assert!(second.status.success(), "{second:?}");

    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".claude/settings.local.json")).expect("settings"),
    )
    .expect("json");
    for (event, command) in [
        ("WorktreeCreate", "newgit claude-code worktree-create"),
        ("WorktreeRemove", "newgit claude-code worktree-remove"),
    ] {
        let entries = settings["hooks"][event].as_array().expect(event);
        assert_eq!(entries.len(), 1, "{event} installed once: {settings}");
        assert_eq!(entries[0]["hooks"][0]["command"], command);
    }
}

#[test]
fn install_refuses_beside_another_create_hook() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    std::fs::create_dir_all(repo.join(".claude")).expect("mkdir");
    let theirs = r#"{"hooks":{"WorktreeCreate":[{"hooks":[{"type":"command","command":"make-worktree"}]}]}}"#;
    std::fs::write(repo.join(".claude/settings.local.json"), theirs).expect("write");

    let output = newgit(&repo, &["claude-code", "install"], "");
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(repo.join(".claude/settings.local.json")).expect("settings"),
        theirs,
        "a refused install leaves the file alone"
    );
}

#[test]
fn create_prints_only_the_workspace_and_remove_deletes_it() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);

    let input = serde_json::json!({
        "hook_event_name": "WorktreeCreate",
        "cwd": repo,
        "name": "bold-oak-a3f2",
    });
    // Run from elsewhere: Claude Code's `cwd` field says where the project is.
    let created = newgit(
        &temp,
        &["claude-code", "worktree-create"],
        &input.to_string(),
    );
    assert!(created.status.success(), "{created:?}");
    let stdout = String::from_utf8(created.stdout).expect("utf8");
    let workspace = Utf8PathBuf::from(stdout.trim());
    assert_eq!(
        stdout.lines().count(),
        1,
        "stdout is the path alone: {stdout:?}"
    );
    assert!(workspace.join("README.md").is_file(), "{workspace}");
    assert!(workspace.join(".newgit/local/instance.toml").is_file());

    let input = serde_json::json!({
        "hook_event_name": "WorktreeRemove",
        "cwd": repo,
        "worktree_path": workspace,
    });
    let removed = newgit(
        &temp,
        &["claude-code", "worktree-remove"],
        &input.to_string(),
    );
    assert!(removed.status.success(), "{removed:?}");
    assert!(removed.stdout.is_empty(), "remove prints nothing on stdout");
    assert!(!workspace.exists());
}

#[test]
fn remove_refuses_the_store_itself() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let input = serde_json::json!({ "worktree_path": repo });
    let output = newgit(
        &repo,
        &["claude-code", "worktree-remove"],
        &input.to_string(),
    );
    assert!(!output.status.success());
    assert!(repo.join("README.md").is_file());
}

/// Claude Code reads the WorktreeRemove hook from the session's own
/// directory, the workspace, so each workspace carries the hooks itself, in
/// a settings file Git never sees.
#[test]
fn create_gives_the_workspace_the_hooks_out_of_git() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);

    let input = serde_json::json!({ "cwd": repo, "name": "feat" });
    let created = newgit(
        &repo,
        &["claude-code", "worktree-create"],
        &input.to_string(),
    );
    assert!(created.status.success(), "{created:?}");
    let workspace = Utf8PathBuf::from(String::from_utf8(created.stdout).expect("utf8").trim());

    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(workspace.join(".claude/settings.local.json"))
            .expect("workspace settings"),
    )
    .expect("json");
    assert_eq!(
        settings["hooks"]["WorktreeRemove"][0]["hooks"][0]["command"],
        "newgit claude-code worktree-remove"
    );
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace.as_str())
        .output()
        .expect("git status");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "",
        "the hooks file is not untracked work"
    );
}

/// `claude --worktree <name>` twice resumes the first instance instead of
/// failing to spawn a second one with the same name.
#[test]
fn create_resumes_an_existing_instance() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let input = serde_json::json!({ "cwd": repo, "name": "feat" }).to_string();

    let first = newgit(&repo, &["claude-code", "worktree-create"], &input);
    assert!(first.status.success(), "{first:?}");
    let second = newgit(&repo, &["claude-code", "worktree-create"], &input);
    assert!(second.status.success(), "{second:?}");
    assert_eq!(first.stdout, second.stdout);
}
