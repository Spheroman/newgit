//! `render --check`: every `[[render]]` resolved against the working tree,
//! before there is an instance or a commit.
//!
//! This is the fix for issue #56. A render's input is committed content —
//! `HEAD`, or a tracker's bound rev — and that must stay true: it is what
//! makes `undo`, `tracker pull`, and re-renders idempotent. But it means the
//! `find` strings you just wrote while adopting a `[[render]]` do not exist
//! in the content newgit would render, so the only feedback loop used to be
//! edit, commit, spawn, read the failure, edit again. `BranchManager::render_check`
//! is the dry run that answers "does this `find` match" against the working
//! tree directly, with no instance and no commit.

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use std::process::Command;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

const SUPABASE_CONFIG: &str = "project_id = \"faretable\"\n\n[api]\nport = 54321\n";

/// A store repo whose committed `supabase/config.toml` holds the project's
/// working defaults, the same shape `render.rs`'s tests use.
fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("supabase")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("supabase/config.toml"), SUPABASE_CONFIG).expect("write");
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

const SUPABASE_RESOURCE: &str = r#"ownership = "branch"

[ports]
api = { start = 54400 }

[[render]]
path = "supabase/config.toml"
replace = [
  { find = 'project_id = "faretable"', with = 'project_id = "faretable-{{branch.slug}}"' },
  { find = "port = 54321",             with = "port = {{ports.api}}" },
]
"#;

fn manager(store: MetadataStore) -> BranchManager {
    BranchManager::open(store).expect("manager")
}

/// The baseline: committed content already satisfies every `find`, with no
/// instance ever spawned.
#[test]
fn check_passes_against_committed_content_with_no_instance_spawned() {
    let (_temp, temp) = tempdir();
    let store = setup(&temp);
    store
        .write_resource_file("supabase", SUPABASE_RESOURCE)
        .expect("resource");
    let manager = manager(store);

    let targets = manager.render_check();
    assert_eq!(targets.len(), 1);
    assert!(targets[0].ok(), "{targets:?}");
    let checks = targets[0].checks.as_ref().expect("checks");
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().all(|check| check.ok()));
}

/// The case the issue is about: a `find` just written to the working tree,
/// not committed, is still visible to `render_check` — unlike a real render,
/// which reads `HEAD` and would not see it yet.
#[test]
fn check_sees_a_find_added_to_the_working_tree_before_it_is_committed() {
    let (_temp, temp) = tempdir();
    let store = setup(&temp);
    // Adopting a second port, uncommitted: mailpit's default, the way the
    // issue's Supabase example added `[local_smtp]`.
    std::fs::write(
        temp.join("store/supabase/config.toml"),
        "project_id = \"faretable\"\n\n[api]\nport = 54321\n\n[local_smtp]\nport = 54324\n",
    )
    .expect("write");
    store
        .write_resource_file(
            "supabase2",
            r#"ownership = "branch"

[ports]
smtp = { start = 54324 }

[[render]]
path = "supabase/config.toml"
replace = [
  { find = "port = 54324", with = "port = {{ports.smtp}}" },
]
"#,
        )
        .expect("resource");
    let manager = manager(store);

    // A real render would fail here: `HEAD` still has no `[local_smtp]`
    // section. `render_check` reads the working tree instead, so it sees
    // the edit that has not been committed yet.
    let targets = manager.render_check();
    let smtp = targets
        .iter()
        .find(|target| target.resource == "supabase2")
        .expect("supabase2 target");
    assert!(smtp.ok(), "{smtp:?}");
}

/// The failure case the issue asks `--check` to name: a `find` that does not
/// match, reported with the file and the string — no instance, no spawn.
#[test]
fn check_reports_a_find_that_does_not_match_the_working_tree() {
    let (_temp, temp) = tempdir();
    let store = setup(&temp);
    // The upstream default moved: the committed file no longer has
    // `port = 54321` at all.
    std::fs::write(
        temp.join("store/supabase/config.toml"),
        "project_id = \"faretable\"\n\n[api]\nport = 55555\n",
    )
    .expect("write");
    store
        .write_resource_file("supabase", SUPABASE_RESOURCE)
        .expect("resource");
    let manager = manager(store);

    let targets = manager.render_check();
    assert_eq!(targets.len(), 1);
    assert!(!targets[0].ok());
    let checks = targets[0].checks.as_ref().expect("checks");
    let port_check = checks
        .iter()
        .find(|check| check.find == "port = 54321")
        .expect("port check present");
    assert!(!port_check.ok());
    assert_eq!(port_check.found, 0);
    assert_eq!(port_check.expected, 1);
    // The other rule in the same file still reports correctly — one bad
    // `find` does not hide the rest of the report.
    let project_check = checks
        .iter()
        .find(|check| check.find.contains("project_id"))
        .expect("project check present");
    assert!(project_check.ok());
}

/// A path the resource declares but that does not exist on disk at all is
/// reported as missing, not silently skipped or panicking.
#[test]
fn check_reports_a_path_missing_from_the_working_tree() {
    let (_temp, temp) = tempdir();
    let store = setup(&temp);
    store
        .write_resource_file(
            "ghost",
            r#"ownership = "branch"

[[render]]
path = "does/not/exist.toml"
replace = [
  { find = "x", with = "y" },
]
"#,
        )
        .expect("resource");
    let manager = manager(store);

    let targets = manager.render_check();
    let ghost = targets
        .iter()
        .find(|target| target.resource == "ghost")
        .expect("ghost target");
    assert!(!ghost.ok());
    assert!(ghost.checks.is_none());
}
