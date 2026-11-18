//! #87: `newgit resource check` says `skip`, with why, for what only a
//! running instance can know, and does not fail on it; and it looks for a
//! relative path where the command runs (`workdir`, but never for
//! `key_command`, which runs at the workspace root).

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
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
    assert!(output.status.success(), "git {args:?} failed");
}

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A project whose `web/bin/serve` is an executable committed under a
/// `web` directory.
fn setup() -> (TempDir, Utf8PathBuf, MetadataStore) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-check-shell-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let temp = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("web/bin")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    let serve = repo.join("web/bin/serve");
    std::fs::write(&serve, "#!/bin/sh\n").expect("write");
    std::fs::set_permissions(&serve, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    (TempDir(temp), repo, store)
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
    )
}

const APP: &str = r#"ownership = "branch"
workdir = "web"

[identity]
paths = ["web"]
key_command = "./web/bin/serve --version"

[actions.serve]
command = "./bin/serve"
long_running = true

[actions.seed]
command = "command -v seed_tool_absent_here >/dev/null && seed_tool_absent_here || echo no seed"

[actions.deploy]
command = "cd \"$TARGET\" && ./deploy.sh {{branch.slug}}"
"#;

#[test]
fn skips_say_why_and_do_not_fail_the_check() {
    let (_guard, repo, store) = setup();
    std::fs::write(store.paths().resources.join("app.toml"), APP).expect("write");

    let (code, stdout) = newgit(&repo, &["resource", "check"]);
    assert_eq!(code, Some(0), "{stdout}");
    for line in [
        "ok    app: [actions.serve] `./bin/serve` (",
        "ok    app: [identity] key_command `./web/bin/serve` (",
        "skip  app: [actions.seed] `seed_tool_absent_here` — not checked: not found on PATH, \
         but the command checks for it first (`command -v seed_tool_absent_here`)",
        "skip  app: [actions.deploy] `./deploy.sh` — not checked: it runs after `cd \"$TARGET\"`, \
         and a variable decides it",
        "2 not checked",
    ] {
        assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    }
    assert!(!stdout.contains("FAIL"), "{stdout}");

    let (code, stdout) = newgit(&repo, &["resource", "check", "--json"]);
    assert_eq!(code, Some(0));
    let json: Value = serde_json::from_str(&stdout).expect("one JSON document");
    let programs = json["programs"].as_array().expect("programs");
    let deploy = programs
        .iter()
        .find(|entry| entry["program"] == "./deploy.sh")
        .expect("deploy listed");
    assert_eq!(deploy["status"], "skip");
    assert!(deploy["found"].is_null());
    assert!(
        deploy["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cd \"$TARGET\"")),
        "{deploy}"
    );
}

#[test]
fn a_command_sh_cannot_parse_fails_the_check() {
    let (_guard, repo, store) = setup();
    std::fs::write(
        store.paths().resources.join("app.toml"),
        "ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"if true; then echo\"\n",
    )
    .expect("write");
    let (code, stdout) = newgit(&repo, &["resource", "check"]);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stdout.contains("FAIL  app: [actions.prepare] — sh cannot parse it: "),
        "{stdout}"
    );
}
