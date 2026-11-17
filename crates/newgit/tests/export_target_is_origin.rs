//! A workspace's `git push` forwards to the store's `origin` as a raw push,
//! with none of an export's checks (#442). So an export target may not be
//! `origin` itself, however its URL is spelled, and the forward refuses a
//! commit a reset (`newgit pull --remote --force`) dropped.

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
    let dir = std::env::temp_dir().join(format!("newgit-origin-target-{}-{n}", std::process::id()));
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

fn set_export(fixture: &Fixture, export: &str) {
    let path = fixture.store.join(".newgit/trackers/root.toml");
    let text = std::fs::read_to_string(&path).expect("definition");
    let mut kept: String = text
        .lines()
        .filter(|line| !line.starts_with("export "))
        .map(|line| format!("{line}\n"))
        .collect();
    kept.push_str(&format!("export = {export}\n"));
    std::fs::write(&path, kept).expect("write");
}

#[test]
fn an_export_target_that_is_origin_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/leak");
    commit(&workspace, "a.txt", "a\n");

    // As `origin` is written, in another spelling, and as Git uses it.
    for url in [
        "github.com/acme/widget".to_owned(),
        "git@github.com:acme/widget".to_owned(),
        fixture.remote.to_string(),
    ] {
        set_export(
            &fixture,
            &format!("{{ url = \"{url}\", name = \"github\" }}"),
        );
        let output = newgit(&fixture.store, &["export", "--remote", "github"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{url}: {stderr}");
        assert!(
            stderr.contains("which is the project's own `origin`"),
            "{url}: {stderr}"
        );

        let push = git_output(&workspace, &["push", "origin", "feature/leak"]);
        let stderr = String::from_utf8_lossy(&push.stderr);
        assert!(!push.status.success(), "{url}: {stderr}");
        assert!(
            stderr.contains("which is the project's own `origin`"),
            "{url}: {stderr}"
        );
        assert_eq!(rev(&fixture.remote, "refs/heads/feature/leak"), None);
    }
}

#[test]
fn a_forward_to_origin_holding_a_dropped_commit_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/dropped");
    let base = git(&workspace, &["rev-parse", "HEAD"]);
    let dropped = commit(&workspace, "key.txt", "sk-live\n");
    commit(&workspace, "b.txt", "b\n");

    // What `newgit pull --remote --force` writes down when it drops a commit.
    std::fs::write(
        fixture.store.join(".newgit/local/resets.json"),
        format!(
            "[{{\"tracker\": \"root\", \"target\": \"github\", \"from\": \"{dropped}\", \
             \"to\": \"{base}\", \"dropped\": [\"{dropped}\"], \"at\": \
             \"2026-11-16T00:00:00Z\"}}]"
        ),
    )
    .expect("write resets");

    let push = git_output(&workspace, &["push", "origin", "feature/dropped"]);
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(!push.status.success(), "{stderr}");
    assert!(
        stderr.contains("this push holds a commit that resetting"),
        "{stderr}"
    );
    assert!(stderr.contains(&dropped[..8]), "{stderr}");
    assert_eq!(rev(&fixture.remote, "refs/heads/feature/dropped"), None);
    assert_eq!(
        git(
            &fixture.remote,
            &["for-each-ref", "--contains", &base, "--format=%(refname)"]
        ),
        "refs/heads/main"
    );

    // Without the record, the same push goes.
    std::fs::remove_file(fixture.store.join(".newgit/local/resets.json")).expect("rm");
    let push = git_output(&workspace, &["push", "origin", "feature/dropped"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
}
