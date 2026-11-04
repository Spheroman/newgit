//! #296: a secret tracker's paths are excluded from a workspace's Git before
//! its content is written there, by every command that writes it — even in
//! a workspace spawned before the tracker was defined.

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
/// returned guard drops — the same hand-rolled guard as `tracker_log.rs`.
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
        "newgit-exclude-before-write-{}-{n}",
        std::process::id()
    ));
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

fn spawn(fixture: &Fixture, name: &str) -> Utf8PathBuf {
    ok(&fixture.store, &["spawn", name]);
    Utf8PathBuf::from(ok(&fixture.store, &["status", name, "--path"]).trim())
}

fn ok(dir: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

const HEADER: &str = "# newgit: tracker-owned paths — secret trackers own this content";

/// Spawn `early` while only `env` exists, then define `site` and give it
/// merged content: `early` predates `site` and has no exclude for it.
/// Returns `early`'s workspace and the rev `site` was captured at.
fn early_instance_and_a_new_tracker(fixture: &Fixture) -> (Utf8PathBuf, String) {
    ok(
        &fixture.store,
        &["tracker", "create", "env", "--audience", "user"],
    );
    ok(&fixture.store, &["tracker", "track", "env", ".env.local"]);
    let early = spawn(fixture, "early");

    ok(&fixture.store, &["tracker", "create", "site"]);
    ok(&fixture.store, &["tracker", "track", "site", "site"]);
    std::fs::create_dir_all(fixture.store.join("site")).expect("mkdir");
    std::fs::write(fixture.store.join("site/index.html"), "<h1>hi</h1>\n").expect("write");
    let captured = ok(
        &fixture.store,
        &["tracker", "capture", "site", "--from-store"],
    );
    let rev = captured
        .split_once(" @ ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .expect("capture names its rev")
        .to_owned();
    (early, rev)
}

fn assert_not_committable(workspace: &Utf8Path) {
    assert!(workspace.join("site/index.html").exists());
    let status = git(
        workspace,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    assert!(
        !status.contains("site/"),
        "Git must not see the secret file: {status}"
    );
    let exclude = std::fs::read_to_string(workspace.join(".git/info/exclude")).expect("exclude");
    assert!(exclude.contains("/site\n"), "{exclude}");
    assert_eq!(
        exclude.matches(HEADER).count(),
        1,
        "one block, not one per write:\n{exclude}"
    );
}

#[test]
fn pull_excludes_a_tracker_the_workspace_predates_before_writing_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (early, _) = early_instance_and_a_new_tracker(&fixture);

    ok(&early, &["tracker", "pull", "site"]);
    assert_not_committable(&early);
}

#[test]
fn checkout_excludes_a_tracker_the_workspace_predates_before_writing_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (early, rev) = early_instance_and_a_new_tracker(&fixture);

    ok(&early, &["tracker", "checkout", "site", "--rev", &rev]);
    assert_not_committable(&early);
}
