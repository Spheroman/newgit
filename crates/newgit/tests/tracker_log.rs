//! `newgit log <tracker>`: one tracker's history, split from the view.

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
    let dir = std::env::temp_dir().join(format!("newgit-tracker-log-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

struct Fixture {
    store: Utf8PathBuf,
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
    Fixture { store: repo }
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

#[test]
fn log_shows_one_trackers_history_for_an_instance_or_the_project_root() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature-a");
    commit(&workspace, "feature.txt", "work\n");
    let checkpoint = newgit(&workspace, &["checkpoint"]);
    assert!(checkpoint.status.success());

    let log = newgit(&workspace, &["log", "root", "--", "--format=%s"]);
    let stdout = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.status.success(),
        "{}",
        String::from_utf8_lossy(&log.stderr)
    );
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        ["feature.txt", "initial"]
    );

    let project_root = newgit(&fixture.store, &["log", "root", "--", "--format=%s"]);
    assert_eq!(
        String::from_utf8_lossy(&project_root.stdout).trim(),
        "initial"
    );
}
