//! An `UndoResourceOutcome` reports the status its resource is left in,
//! not just whether the restore action succeeded. The two differ exactly
//! when the restore ran nothing: `ok` is true, and the resource keeps the
//! status it had — a failed `prepare` stays failed, a dependent whose
//! `prepare` never ran stays pending (#211).

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::ResourceStatus;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, UndoOptions};
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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

const NEVER_PREPARES: &str = r#"ownership = "workspace"

[actions.prepare]
command = "exit 1"
"#;

const FINE: &str = r#"ownership = "workspace"

[actions.prepare]
command = "true"
"#;

const DEPENDENT: &str = r#"ownership = "workspace"
depends_on = ["deps"]

[actions.prepare]
command = "true"
"#;

#[test]
fn an_ok_restore_that_ran_nothing_reports_the_status_it_left() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "deps", NEVER_PREPARES);
    write_resource(&store, "fine", FINE);
    write_resource(&store, "dependent", DEPENDENT);
    let m = BranchManager::open(store).expect("manager");
    let spawned = m.spawn("feature-a", None).expect("spawn");
    assert!(
        spawned.failed_resources().contains(&"deps"),
        "{:?}",
        spawned.resources
    );
    m.checkpoint("feature-a", None).expect("checkpoint");

    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");
    assert!(undo.is_complete(), "{:?}", undo.resources);

    let outcome = |name: &str| {
        undo.resources
            .iter()
            .find(|resource| resource.name == name)
            .unwrap_or_else(|| panic!("`{name}` in {:?}", undo.resources))
    };
    let branch = m.store().find_branch("feature-a").expect("instance");
    for (name, status, blocked_by) in [
        ("deps", ResourceStatus::Failed, &[][..]),
        ("fine", ResourceStatus::Ready, &[]),
        ("dependent", ResourceStatus::Pending, &["deps"]),
    ] {
        let resource = outcome(name);
        assert!(resource.ok, "`{name}`: nothing ran, so nothing failed");
        assert!(!resource.restart_failed, "`{name}`: nothing was running");
        assert_eq!(resource.action, "none", "`{name}`");
        assert_eq!(resource.status, status, "`{name}`");
        assert_eq!(resource.blocked_by, blocked_by, "`{name}`");
        // The outcome says what the record says.
        assert_eq!(branch.resources[name].status, status, "`{name}`");
    }
}

/// A restore that is ok can still leave its resource failed: one running at
/// checkpoint time whose start then fails. The outcome's status is read
/// after that failure marks it, so it says `Failed`, not `Ready`.
#[test]
fn an_ok_restore_whose_restart_fails_reports_failed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let resource = |workdir: &str| {
        format!(
            r#"ownership = "workspace"

[actions.start]
command = "sleep 30"
long_running = true
workdir = "{workdir}"

[actions.stop]
signal = "term"
"#
        )
    };
    write_resource(&store, "app", &resource("."));
    let resources = store.paths().resources.clone();
    let m = BranchManager::open(store).expect("manager");
    m.spawn("feature-a", None).expect("spawn");
    m.run_action("feature-a", "app.start").expect("start");
    let checkpoint = m.checkpoint("feature-a", None).expect("checkpoint");
    assert!(checkpoint.record.resource_states[0].was_running);

    // Point the start at a workdir that does not exist, so the restart the
    // undo attempts fails before anything runs.
    std::fs::write(resources.join("app.toml"), resource("missing")).expect("rewrite");
    let m = BranchManager::open(m.store().clone()).expect("reopen");
    let undo = m.undo("feature-a", &UndoOptions::default()).expect("undo");

    let app = &undo.resources[0];
    assert_eq!(app.name, "app");
    assert!(app.ok, "the restore itself ran nothing: {app:?}");
    assert!(app.restart_failed, "{app:?}");
    assert_eq!(app.status, ResourceStatus::Failed, "{app:?}");
    assert!(!undo.is_complete());
    assert!(undo.recovery_record.is_some());
    let branch = m.store().find_branch("feature-a").expect("instance");
    assert_eq!(branch.resources["app"].status, ResourceStatus::Failed);
}
