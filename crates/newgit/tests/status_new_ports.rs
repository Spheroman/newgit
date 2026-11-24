//! #85: a port added to a definition after an instance was spawned is
//! claimed by no record. `status` says so and names the fix, and `resource
//! rebind` claims it.

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
    assert!(output.status.success(), "git {args:?} failed");
}

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn a_port_declared_after_spawn_is_named_until_rebind_claims_it() {
    let dir = std::env::temp_dir().join(format!("newgit-newports-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let temp = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
    let _guard = TempDir(temp.clone());
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
    let definition = store.paths().resources.join("stack.toml");
    std::fs::write(
        &definition,
        "ownership = \"branch\"\n\n[ports]\napi = { start = 55210 }\n",
    )
    .expect("write");

    let (ok, out) = newgit(&repo, &["spawn", "x"]);
    assert!(ok, "{out}");
    let (_, status) = newgit(&repo, &["status", "x"]);
    assert!(!status.contains("was bound from an older"), "{status}");

    std::fs::write(
        &definition,
        "ownership = \"branch\"\n\n[ports]\napi = { start = 55210 }\ninbucket = { start = 55220 }\nanalytics = { start = 55230 }\n",
    )
    .expect("write");
    let (_, status) = newgit(&repo, &["status", "x"]);
    assert!(
        status.contains("ports `analytics`, `inbucket` declared since, so nothing claims them"),
        "{status}"
    );
    assert!(status.contains("newgit resource rebind stack"), "{status}");

    let (ok, out) = newgit(&repo, &["resource", "rebind", "stack", "x"]);
    assert!(ok, "{out}");
    let (_, status) = newgit(&repo, &["status", "x"]);
    assert!(!status.contains("was bound from an older"), "{status}");
    let ports = &store.find_branch("x").expect("x").resources["stack"].resolved_ports;
    assert!(ports.contains_key("inbucket") && ports.contains_key("analytics"));
}
