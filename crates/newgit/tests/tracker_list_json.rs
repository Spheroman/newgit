//! `newgit tracker list --json`: every tracker, with what a new instance binds
//! by default and what this instance binds — what a tool needs to offer the
//! choice `spawn --use`/`--drop` make (T3 Code's new-worktree checkboxes).

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::Value;

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
        "newgit-tracker-list-json-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> Utf8PathBuf {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("docs")).expect("mkdir");
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
    for (name, contents) in [
        (
            "notes",
            "audience = \"user\"\nstorage = \"local\"\npaths = [\"docs\"]\n",
        ),
        (
            "extra",
            "audience = \"user\"\nstorage = \"local\"\npaths = []\npriority = -1\n",
        ),
        (
            "dev-db",
            "audience = \"user\"\nstorage = \"local\"\npaths = []\nrole = \"db\"\npriority = 1\n",
        ),
        (
            "prod-db",
            "audience = \"user\"\nstorage = \"local\"\npaths = []\nrole = \"db\"\n",
        ),
    ] {
        std::fs::write(
            store.paths().trackers.join(format!("{name}.toml")),
            contents,
        )
        .expect("write tracker");
    }
    repo
}

fn newgit_json(dir: &Utf8Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON stdout")
}

fn tracker<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["trackers"]
        .as_array()
        .expect("trackers")
        .iter()
        .find(|tracker| tracker["name"] == name)
        .unwrap_or_else(|| panic!("no `{name}` in {list:#}"))
}

#[test]
fn reports_defaults_outside_an_instance() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let list = newgit_json(&repo, &["tracker", "list", "--json"]);

    assert_eq!(list["instance"], Value::Null);
    let root = tracker(&list, "root");
    assert_eq!(root["selectable"], false);
    assert_eq!(root["default_bound"], true);
    assert_eq!(root["bound"], Value::Null);

    assert_eq!(tracker(&list, "notes")["default_bound"], true);
    assert_eq!(tracker(&list, "notes")["selectable"], true);
    // Negative priority: only when asked for.
    assert_eq!(tracker(&list, "extra")["default_bound"], false);
    assert_eq!(tracker(&list, "extra")["priority"], -1);
    // One per role, the highest priority.
    assert_eq!(tracker(&list, "dev-db")["default_bound"], true);
    assert_eq!(tracker(&list, "prod-db")["default_bound"], false);
    assert_eq!(tracker(&list, "prod-db")["role"], "db");
}

#[test]
fn reports_what_an_instance_binds() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let spawned = newgit_json(
        &repo,
        &[
            "spawn", "feat", "--use", "extra", "--use", "prod-db", "--drop", "notes", "--json",
        ],
    );
    let workspace = Utf8PathBuf::from(spawned["workspace"].as_str().expect("workspace"));
    let list = newgit_json(&workspace, &["tracker", "list", "--json"]);

    assert_eq!(list["instance"], "feat");
    assert_eq!(tracker(&list, "extra")["bound"], true);
    assert_eq!(tracker(&list, "notes")["bound"], false);
    assert_eq!(tracker(&list, "prod-db")["bound"], true);
    assert_eq!(tracker(&list, "dev-db")["bound"], false);
    // Defaults do not move with what this instance chose.
    assert_eq!(tracker(&list, "notes")["default_bound"], true);
    assert_eq!(tracker(&list, "extra")["default_bound"], false);
}
