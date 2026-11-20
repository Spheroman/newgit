//! #87: `newgit resource check` finds a program a definition's command
//! starts that this machine does not have, before the checkpoint or the
//! restore that would have run it.

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

fn setup() -> (TempDir, Utf8PathBuf, MetadataStore) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-check-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let temp = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("scripts")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("scripts/seed.sh"), "#!/bin/sh\n").expect("write");
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

const DB: &str = r#"ownership = "branch"

[actions.prepare]
command = "sh ./scripts/seed.sh && ./scripts/missing.sh"

[checkpoint]
mode = "command"
command = "pg_dump_absent_here --data-only \"$DATABASE_URL\" > {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "psql_absent_here < {{state_ref}}"
"#;

#[test]
fn a_missing_program_fails_naming_its_resource_and_hook() {
    let (_guard, repo, store) = setup();
    std::fs::write(store.paths().resources.join("db.toml"), DB).expect("write");
    std::fs::write(
        store.paths().trackers.join("db-snapshots.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = []\n",
    )
    .expect("write tracker");

    let (code, stdout) = newgit(&repo, &["resource", "check"]);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stdout.contains("FAIL  db: [checkpoint] `pg_dump_absent_here` — not found on PATH"),
        "{stdout}"
    );
    assert!(
        stdout.contains("FAIL  db: [restore] `psql_absent_here` — not found on PATH"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "FAIL  db: [actions.prepare] `./scripts/missing.sh` — not found in the project root"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("ok    db: [actions.prepare] `sh` ("),
        "{stdout}"
    );
    assert!(stdout.contains("3 programs missing"), "{stdout}");

    let (code, stdout) = newgit(&repo, &["resource", "check", "--json"]);
    assert_eq!(code, Some(1));
    let json: Value = serde_json::from_str(&stdout).expect("one JSON document");
    let programs = json["programs"].as_array().expect("programs");
    let pg_dump = programs
        .iter()
        .find(|entry| entry["program"] == "pg_dump_absent_here")
        .expect("pg_dump listed");
    assert_eq!(pg_dump["hook"], "[checkpoint]");
    assert!(pg_dump["found"].is_null());
}

#[test]
fn everything_found_exits_zero() {
    let (_guard, repo, store) = setup();
    std::fs::write(
        store.paths().resources.join("app.toml"),
        "ownership = \"branch\"\n\n[actions.serve]\ncommand = \"exec sh ./scripts/seed.sh\"\nlong_running = true\n",
    )
    .expect("write");
    let (code, stdout) = newgit(&repo, &["resource", "check"]);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(
        stdout.contains("ok    app: [actions.serve] `sh`"),
        "{stdout}"
    );
}
