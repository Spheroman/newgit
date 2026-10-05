//! `[remote]` in `.newgit/config.toml`: where a project syncs to. Nothing
//! talks to the network yet; these pin the shape and the refusals.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A store whose config.toml gets `remote` appended verbatim.
fn store_with(temp: &Utf8Path, remote: &str) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "acme", SourceSubstrate::Git).expect("init");
    let config = store.paths().config.clone();
    let mut contents = std::fs::read_to_string(&config).expect("read config");
    contents.push_str(remote);
    std::fs::write(&config, contents).expect("write config");
    store
}

#[test]
fn a_remote_project_is_owner_slash_name_once_remote_add_set_it() {
    // A url alone: the project is not on the remote yet.
    let (_guard, temp) = tempdir();
    let store = store_with(
        &temp,
        "\n[remote]\nurl = \"https://newgit.example.workers.dev\"\n",
    );
    assert_eq!(store.load_config().expect("load").remote_project(), None);

    let (_guard, temp) = tempdir();
    let store = store_with(
        &temp,
        "\n[remote]\nurl = \"https://newgit.example.workers.dev\"\nproject = \"jack/acme-web\"\n",
    );
    assert_eq!(
        store.load_config().expect("load").remote_project(),
        Some("jack/acme-web")
    );

    let (_guard, temp) = tempdir();
    let store = store_with(&temp, "");
    assert_eq!(store.load_config().expect("load").remote_project(), None);
}

#[test]
fn plain_http_is_refused_except_for_a_local_worker() {
    let (_guard, temp) = tempdir();
    let store = store_with(
        &temp,
        "\n[remote]\nurl = \"http://newgit.example.workers.dev\"\n",
    );
    let error = store.load_config().expect_err("plain http to a real host");
    assert!(error.to_string().contains("must be https://"), "{error}");

    for url in ["http://localhost:8787", "http://127.0.0.1:8787/"] {
        let (_guard, temp) = tempdir();
        let store = store_with(&temp, &format!("\n[remote]\nurl = \"{url}\"\n"));
        store
            .load_config()
            .unwrap_or_else(|error| panic!("{url} is a local wrangler dev: {error}"));
    }

    // Not a prefix trick: localhost.evil.example is not localhost.
    let (_guard, temp) = tempdir();
    let store = store_with(
        &temp,
        "\n[remote]\nurl = \"http://localhost.evil.example\"\n",
    );
    assert!(store.load_config().is_err());
}

#[test]
fn a_project_the_remote_cannot_name_is_refused_with_the_fix() {
    for project in ["-acme", "acme", "jack/ac.me", "jack/acme/extra"] {
        let (_guard, temp) = tempdir();
        let store = store_with(
            &temp,
            &format!(
                "\n[remote]\nurl = \"https://newgit.example.workers.dev\"\nproject = \"{project}\"\n"
            ),
        );
        let error = store.load_config().expect_err(project);
        assert!(error.to_string().contains("<owner>/<name>"), "{error}");
    }
}

#[test]
fn an_unknown_remote_key_is_an_error_not_ignored() {
    let (_guard, temp) = tempdir();
    let store = store_with(
        &temp,
        "\n[remote]\nurl = \"https://newgit.example.workers.dev\"\ntoken = \"oops\"\n",
    );
    assert!(store.load_config().is_err());
}

#[test]
fn a_remote_tracker_needs_somewhere_to_sync() {
    let (_guard, temp) = tempdir();
    let manager = BranchManager::open(store_with(&temp, "")).expect("manager");
    let error = manager
        .create_tracker("jack-env", "user:jack", Storage::Remote, false)
        .expect_err("no [remote]");
    let message = error.to_string();
    assert!(message.contains("no [remote]"), "{message}");
    assert!(
        message.contains("--storage local"),
        "names the way out: {message}"
    );
    manager
        .create_tracker("jack-env", "user:jack", Storage::Local, false)
        .expect("local is fine");

    let (_guard, temp) = tempdir();
    let manager = BranchManager::open(store_with(
        &temp,
        "\n[remote]\nurl = \"https://newgit.example.workers.dev\"\n",
    ))
    .expect("manager");
    manager
        .create_tracker("jack-env", "user:jack", Storage::Remote, false)
        .expect("a configured remote accepts it");
}
