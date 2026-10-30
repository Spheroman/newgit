//! Regression tests for #263: an over-long instance name is refused with a
//! clear message, not a raw "File name too long" from the lock file, and
//! `render --check` reports the environment-name collisions `spawn` refuses.
//!
//! New tests live in their own file per AGENTS.md "Working in parallel".

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

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops. Hand-rolled rather than pulling in the `tempfile`
/// dev-dependency other crates use — matches `table_alignment.rs`, the only
/// other test file in this crate.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-spawn-exit-{}-{n}", std::process::id()));
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
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

fn run(repo: &Utf8Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

const PORT_ON_WEB: &str = r#"ownership = "branch"

[ports]
web = { start = 4100, env = "WEB_PORT" }
"#;

const EXPORTS_WEB_URL: &str = r#"ownership = "branch"

[exports]
WEB_URL = "http://127.0.0.1:1"
"#;

#[test]
fn a_name_over_the_limit_is_refused_with_a_message_not_an_io_error() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    for len in [41, 200, 300] {
        let out = run(&repo, &["spawn", &"m".repeat(len)]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success());
        assert!(
            stderr.contains("character limit") && !stderr.contains("File name too long"),
            "{len}: {stderr}"
        );
    }
}

#[test]
fn a_name_at_the_limit_still_spawns() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    let name = "m".repeat(newgit_core::branch::MAX_NAME_LEN);
    let out = run(&repo, &["spawn", &name]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn render_check_and_resource_list_flag_env_name_collisions() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "a", PORT_ON_WEB);
    write_resource(&store, "b", PORT_ON_WEB);
    write_resource(&store, "c", EXPORTS_WEB_URL);
    write_resource(&store, "d", EXPORTS_WEB_URL);
    let repo = store.paths().project_root.clone();

    for args in [&["render", "--check"][..], &["resource", "list"][..]] {
        let out = run(&repo, args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        for name in ["WEB_PORT", "WEB_URL"] {
            assert!(
                stderr.contains(&format!(
                    "warning: environment variable `{name}` is declared more than once"
                )),
                "{args:?}: {stderr}"
            );
        }
        assert!(stderr.contains("will refuse until it resolves"), "{stderr}");
    }

    // And spawn refuses, with the same words.
    let out = run(&repo, &["spawn", "x"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("`WEB_PORT` is declared more than once"));
}
