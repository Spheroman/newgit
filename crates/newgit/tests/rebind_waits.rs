//! `newgit resource rebind` waits for what it started, as `spawn` and
//! `start` do (#276), and exits 1 when the instance it leaves is not ready.
//! Its help said so, but it returned 0 at once: an agent rebinding a server
//! that never came up was told it had.

use std::process::Command;

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
    let dir = std::env::temp_dir().join(format!("newgit-rebind-wait-{}-{n}", std::process::id()));
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
    std::fs::write(store.paths().resources.join("api.toml"), API).expect("write resource");
    repo
}

/// Runs, and is up only once `up` exists in its workspace.
const API: &str = r#"ownership = "branch"

[actions.serve]
command = "exec sleep 600"
long_running = true
autostart = true

[ready]
command = "test -f up"
timeout_seconds = 2
"#;

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn rebind_waits_and_exits_1_when_not_up() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let (code, said) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(1), "{said}");
    let (code, said) = newgit(&repo, &["stop", "x"]);
    assert_eq!(code, Some(0), "{said}");

    let began = std::time::Instant::now();
    let (code, said) = newgit(&repo, &["resource", "rebind", "api", "x"]);
    assert_eq!(code, Some(1), "{said}");
    assert!(
        began.elapsed() >= std::time::Duration::from_secs(2),
        "it waited its `[ready] timeout_seconds`: {:?}",
        began.elapsed()
    );
    assert!(said.contains("not ready: `x`"), "{said}");
    assert!(said.contains("not up after 2s"), "{said}");
}

#[test]
fn rebind_exits_0_once_up() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    newgit(&repo, &["spawn", "x"]);
    newgit(&repo, &["stop", "x"]);
    let workspace = temp.join("workspaces").join("x");
    assert!(workspace.is_dir(), "{workspace}");
    std::fs::write(workspace.join("up"), "").expect("write");

    let (code, said) = newgit(&repo, &["resource", "rebind", "api", "x"]);
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("ready: `x`"), "{said}");
}
