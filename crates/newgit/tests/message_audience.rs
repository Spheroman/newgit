//! A commit's message is copied into every tracker it changes, so a commit
//! that changes trackers with different audiences is refused where it would
//! leave the machine: one commit per audience, so each message stays with
//! the work it describes.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

const GITHUB_URL: &str = "https://github.com/acme/widget.git";

fn git_output(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs")
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
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
        "newgit-message-audience-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn newgit(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

fn newgit_ok(dir: &Utf8Path, args: &[&str]) {
    let output = newgit(dir, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A project pushing to a stand-in for GitHub, with `billing/` in a code
/// tracker only `jack` may read, and `docs/` in one with root's audience.
/// Returns the store, the stand-in, and `feature-a`'s workspace.
fn setup(temp: &Utf8Path) -> (Utf8PathBuf, Utf8PathBuf, Utf8PathBuf) {
    let remote = temp.join("github.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", remote.as_str()],
    );
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("billing")).expect("mkdir");
    std::fs::create_dir_all(repo.join("docs")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("billing/charge.rs"), "fn charge() {}\n").expect("write");
    std::fs::write(repo.join("docs/guide.md"), "guide\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    git(&repo, &["remote", "add", "origin", GITHUB_URL]);
    git(
        &repo,
        &["config", &format!("url.{remote}.insteadOf"), GITHUB_URL],
    );
    git(&repo, &["push", "-q", "origin", "main"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    newgit_ok(
        &repo,
        &[
            "tracker",
            "create",
            "billing",
            "--code",
            "--path",
            "billing",
            "--audience",
            "user:jack",
        ],
    );
    newgit_ok(
        &repo,
        &["tracker", "create", "docs", "--code", "--path", "docs"],
    );
    newgit_ok(&repo, &["spawn", "feature-a"]);
    let path = newgit(&repo, &["status", "feature-a", "--path"]);
    let workspace = Utf8PathBuf::from(String::from_utf8_lossy(&path.stdout).trim());
    (repo, remote, workspace)
}

fn edit(workspace: &Utf8Path, file: &str, contents: &str) {
    std::fs::write(workspace.join(file), contents).expect("write");
    git(workspace, &["add", file]);
}

fn remote_has(remote: &Utf8Path, branch: &str) -> bool {
    git(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    )
    .contains(branch)
}

#[test]
fn a_commit_changing_trackers_with_different_audiences_is_refused() {
    let (_guard, temp) = tempdir();
    let (_repo, remote, workspace) = setup(&temp);

    edit(&workspace, "billing/charge.rs", "fn charge() { check() }\n");
    edit(&workspace, "README.md", "hello again\n");
    git(&workspace, &["commit", "-q", "-m", "Fix the refund bypass"]);
    let output = git_output(&workspace, &["push", "-q", "origin", "feature-a"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the push should be refused");
    assert!(
        stderr.contains("Fix the refund bypass")
            && stderr.contains("`billing` (user:jack)")
            && stderr.contains("`root` (project-devs)")
            && stderr.contains("one commit per audience"),
        "{stderr}"
    );
    assert!(!remote_has(&remote, "feature-a"));
}

#[test]
fn one_commit_per_audience_pushes() {
    let (_guard, temp) = tempdir();
    let (_repo, remote, workspace) = setup(&temp);

    edit(&workspace, "billing/charge.rs", "fn charge() { check() }\n");
    git(&workspace, &["commit", "-q", "-m", "Fix the refund bypass"]);
    edit(&workspace, "README.md", "hello again\n");
    git(&workspace, &["commit", "-q", "-m", "Reword the README"]);
    git(&workspace, &["push", "-q", "origin", "feature-a"]);
    assert!(remote_has(&remote, "feature-a"));
}

#[test]
fn a_commit_across_trackers_with_one_audience_pushes() {
    let (_guard, temp) = tempdir();
    let (_repo, remote, workspace) = setup(&temp);

    edit(&workspace, "docs/guide.md", "a better guide\n");
    edit(&workspace, "README.md", "hello again\n");
    git(&workspace, &["commit", "-q", "-m", "Document the API"]);
    git(&workspace, &["push", "-q", "origin", "feature-a"]);
    assert!(remote_has(&remote, "feature-a"));
}
