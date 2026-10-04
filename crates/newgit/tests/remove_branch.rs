//! #82: `remove` deletes the source branch when it holds nothing unique, and
//! says why it kept it otherwise. The case only the CLI can show end to end:
//! a branch published with a workspace `git push` lives on the real remote,
//! which the store learns about through newgit's outbox rather than its own
//! `refs/remotes` — and it still counts as "exists elsewhere".

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
    let dir = std::env::temp_dir().join(format!("newgit-remove-branch-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

struct Fixture {
    store: Utf8PathBuf,
    remote: Utf8PathBuf,
}

fn setup(temp: &Utf8Path) -> Fixture {
    let remote = temp.join("github.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", remote.as_str()],
    );

    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
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
    Fixture {
        store: repo,
        remote,
    }
}

fn newgit(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

fn spawn(fixture: &Fixture, name: &str) -> (Utf8PathBuf, String) {
    let output = newgit(&fixture.store, &["spawn", name]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "spawn failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = newgit(&fixture.store, &["status", name, "--path"]);
    let workspace = Utf8PathBuf::from(String::from_utf8_lossy(&path.stdout).trim());
    (workspace, stdout)
}

fn commit(workspace: &Utf8Path, file: &str, contents: &str) -> String {
    std::fs::write(workspace.join(file), contents).expect("write");
    git(workspace, &["add", file]);
    git(workspace, &["commit", "-q", "-m", file]);
    git(workspace, &["rev-parse", "HEAD"])
}

fn rev(repo: &Utf8Path, name: &str) -> Option<String> {
    let output = git_output(repo, &["rev-parse", "--verify", "--quiet", name]);
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn remove(fixture: &Fixture, name: &str, extra: &[&str]) -> String {
    let mut args = vec!["remove", name];
    args.extend_from_slice(extra);
    let output = newgit(&fixture.store, &args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "remove failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

#[test]
fn a_branch_published_by_a_workspace_push_is_deleted() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/pushed");
    commit(&workspace, "a.txt", "a\n");
    let push = git_output(&workspace, &["push", "origin", "feature/pushed"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(rev(&fixture.remote, "refs/heads/feature/pushed").is_some());
    assert_eq!(
        rev(&fixture.store, "refs/remotes/origin/feature/pushed"),
        None,
        "the store's own remote-tracking refs never hear about the push"
    );

    let stdout = remove(&fixture, "feature/pushed", &[]);
    assert!(
        stdout.contains("`feature/pushed` deleted")
            && stdout.contains("`origin/feature/pushed` (as of the last push)"),
        "{stdout}"
    );
    assert_eq!(rev(&fixture.store, "refs/heads/feature/pushed"), None);
}

#[test]
fn an_unpublished_branch_is_kept_and_the_reason_printed() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/local");
    let tip = commit(&workspace, "a.txt", "a\n");
    let checkpoint = newgit(&fixture.store, &["checkpoint", "feature/local"]);
    assert!(checkpoint.status.success());

    let stdout = remove(&fixture, "feature/local", &[]);
    assert!(
        stdout.contains("is on no other branch, tag, or remote")
            && stdout.contains("git branch -D feature/local"),
        "{stdout}"
    );
    assert_eq!(rev(&fixture.store, "refs/heads/feature/local"), Some(tip));
}

#[test]
fn delete_branch_prints_how_to_get_the_commits_back() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/forced");
    let tip = commit(&workspace, "a.txt", "a\n");
    let checkpoint = newgit(&fixture.store, &["checkpoint", "feature/forced"]);
    assert!(checkpoint.status.success());

    let stdout = remove(&fixture, "feature/forced", &["--delete-branch"]);
    assert!(
        stdout.contains(&format!("git branch feature/forced {tip}")),
        "{stdout}"
    );
    assert_eq!(rev(&fixture.store, "refs/heads/feature/forced"), None);
}

#[test]
fn keep_branch_and_delete_branch_conflict() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    spawn(&fixture, "feature/both");
    let output = newgit(
        &fixture.store,
        &["remove", "feature/both", "--keep-branch", "--delete-branch"],
    );
    assert!(!output.status.success());
}
