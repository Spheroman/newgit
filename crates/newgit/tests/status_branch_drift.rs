//! `status` and `checkpoints` show a workspace that is off its instance's
//! branch (#259), instead of labelling another branch's commit as the
//! instance's.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

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
    let dir = std::env::temp_dir().join(format!("newgit-drift-{}-{n}", std::process::id()));
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

fn newgit(repo: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .env_remove("NEWGIT_LOCKED")
        .args(args)
        .output()
        .expect("newgit runs");
    assert!(
        output.status.success(),
        "newgit {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn status_and_checkpoints_name_the_branch_the_workspace_is_on() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    newgit(&repo, &["spawn", "base"]);
    let ws = Utf8PathBuf::from(newgit(&repo, &["status", "base", "--path"]).trim());
    let base = git(&repo, &["rev-parse", "refs/heads/base"]);

    git(&ws, &["checkout", "-q", "-b", "sneaky"]);
    git(&ws, &["commit", "-q", "--allow-empty", "-m", "sneaky"]);
    let checkpoint = newgit(&repo, &["checkpoint", "base"]);
    assert!(
        checkpoint.contains("`sneaky`") && checkpoint.contains("not `base`"),
        "{checkpoint}"
    );

    let status = newgit(&repo, &["status", "base"]);
    assert!(
        status.contains(&format!("base@{} (workspace on `sneaky`)", &base[..8])),
        "{status}"
    );
    let listed = newgit(&repo, &["checkpoints", "base"]);
    assert!(listed.contains("[on `sneaky`]"), "{listed}");

    git(&ws, &["checkout", "-q", "--detach"]);
    let head = git(&ws, &["rev-parse", "--short", "HEAD"]);
    let status = newgit(&repo, &["status", "base"]);
    assert!(
        status.contains(&format!("(workspace detached at {head})")),
        "{status}"
    );
}
