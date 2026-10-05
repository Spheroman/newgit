//! Step 3c: work is split into its trackers when it leaves the machine, and
//! a secret tracker's file never leaves with it — on any branch a push sends.

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
    let dir = std::env::temp_dir().join(format!("newgit-push-secrets-{}-{n}", std::process::id()));
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

fn git_fails(dir: &Utf8Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(!output.status.success(), "git {args:?} should have failed");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A project with a secret tracker owning `.env.local`.
fn with_secret_tracker(fixture: &Fixture) {
    for args in [
        &["tracker", "create", "env", "--audience", "user"][..],
        &["tracker", "track", "env", ".env.local"][..],
    ] {
        let output = newgit(&fixture.store, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn remote_branches(fixture: &Fixture) -> String {
    git(
        &fixture.remote,
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    )
}

#[test]
fn a_push_of_the_instances_branch_holding_a_secret_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    with_secret_tracker(&fixture);
    let (workspace, _) = spawn(&fixture, "feature-a");

    std::fs::write(workspace.join(".env.local"), "TOKEN=sk_live_do_not_leak\n").expect("write");
    git(&workspace, &["add", "-f", ".env.local"]);
    git(&workspace, &["commit", "-q", "-m", "oops"]);
    let stderr = git_fails(&workspace, &["push", "-q", "origin", "feature-a"]);
    assert!(
        stderr.contains(".env.local") && stderr.contains("`env`"),
        "{stderr}"
    );
    assert!(!remote_branches(&fixture).contains("feature-a"));
}

#[test]
fn a_push_of_any_other_branch_holding_a_secret_is_refused_too() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    with_secret_tracker(&fixture);
    let (workspace, _) = spawn(&fixture, "feature-a");

    git(&workspace, &["checkout", "-q", "-b", "side"]);
    std::fs::write(workspace.join(".env.local"), "TOKEN=sk_live_do_not_leak\n").expect("write");
    git(&workspace, &["add", "-f", ".env.local"]);
    git(&workspace, &["commit", "-q", "-m", "oops"]);
    // A clean commit on top does not hide it: every new commit is checked.
    commit(&workspace, "notes.txt", "fine\n");
    let stderr = git_fails(&workspace, &["push", "-q", "origin", "side"]);
    assert!(stderr.contains(".env.local"), "{stderr}");
    assert!(!remote_branches(&fixture).contains("side"));
}

#[test]
fn a_clean_push_is_split_into_the_instances_tracker_branches() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    with_secret_tracker(&fixture);
    let (workspace, _) = spawn(&fixture, "feature-a");

    let pushed = commit(&workspace, "feature.txt", "work\n");
    git(&workspace, &["push", "-q", "origin", "feature-a"]);
    assert!(remote_branches(&fixture).contains("feature-a"));

    let trackers = fixture.store.join(".newgit/local/trackers.git");
    let root = git(
        &trackers,
        &[
            "log",
            "-1",
            "--format=%B",
            "refs/trackers/root/instances/feature-a",
        ],
    );
    assert!(
        root.contains(&format!("Newgit-Split: {pushed}")),
        "root's branch holds the pushed commit, split: {root}"
    );
}

#[test]
fn history_from_before_newgit_never_blocks_a_push_to_a_fresh_remote() {
    let (_guard, temp) = tempdir();
    let remote = temp.join("github.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", remote.as_str()],
    );
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join(".env.local"), "TOKEN=old\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "years ago, by mistake"]);
    git(&repo, &["rm", "-q", "--cached", ".env.local"]);
    std::fs::write(repo.join(".gitignore"), ".env.local\n").expect("write");
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-q", "-m", "stop tracking it"]);
    git(&repo, &["remote", "add", "origin", GITHUB_URL]);
    git(
        &repo,
        &["config", &format!("url.{remote}.insteadOf"), GITHUB_URL],
    );
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    let fixture = Fixture {
        store: repo,
        remote,
    };
    with_secret_tracker(&fixture);
    let (workspace, _) = spawn(&fixture, "feature-a");

    commit(&workspace, "feature.txt", "work\n");
    let output = git_output(&workspace, &["push", "-q", "origin", "feature-a"]);
    assert!(
        output.status.success(),
        "the old `.env` commit is history, not new work: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(remote_branches(&fixture).contains("feature-a"));
}
