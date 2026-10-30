//! `tracker use` of a secret tracker the instance has never bound — dropped
//! at spawn because another tracker filled its role — puts the tracker's
//! main in the workspace, as `spawn --use` does. It used to restore only a
//! rev the instance already held for it, so it bound the tracker with no
//! files, and a resource it brought failed for want of a file to render.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::ResourceStatus;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BindingChoice, BranchManager};
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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

const SHARED_ENV: &str = "DATABASE_URL=postgres://shared.example.com:5432/app\n";
const BRANCH_ENV: &str = "DATABASE_URL=postgres://127.0.0.1:54322/app\n";

/// Two secret trackers in the `db` role: `shared-db` wins it by priority,
/// `branch-db` brings `fakedb`, whose render points `.env.db` at its port.
/// With `seed`, each lane's main holds its own `.env.db`; without, neither
/// has any content yet.
fn project(temp: &Utf8Path, seed: bool) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join(".gitignore"), "/.env.db\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");

    let trackers = &store.paths().trackers;
    std::fs::write(
        trackers.join("shared-db.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.db\"]\nrole = \"db\"\npriority = 10\n",
    )
    .expect("write");
    std::fs::write(
        trackers.join("branch-db.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.db\"]\nrole = \"db\"\nresources = [\"fakedb\"]\n",
    )
    .expect("write");
    std::fs::write(
        store.paths().resources.join("fakedb.toml"),
        r#"ownership = "branch"

[ports.db]
start = 55000

[actions.prepare]
command = "true"

[[render]]
path = ".env.db"
replace = [
  { find = "127.0.0.1:54322", with = "127.0.0.1:{{ports.db}}" },
]
"#,
    )
    .expect("resource");

    let manager = BranchManager::open(store).expect("manager");
    if seed {
        for (tracker, content) in [("shared-db", SHARED_ENV), ("branch-db", BRANCH_ENV)] {
            std::fs::write(repo.join(".env.db"), content).expect("write");
            manager.seed_tracker_from_store(tracker).expect("seed");
        }
        std::fs::remove_file(repo.join(".env.db")).expect("remove");
    }
    manager
}

fn port(manager: &BranchManager, instance: &str) -> u16 {
    manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .resources["fakedb"]
        .resolved_ports["db"]
}

#[test]
fn using_a_tracker_never_bound_here_brings_its_main_as_spawn_does() {
    let (_guard, temp) = tempdir();
    let manager = project(&temp, true);

    // What `spawn --use branch-db` leaves: the baseline `use` has to match.
    let spawned = manager
        .spawn_binding(
            "b",
            None,
            &BindingChoice {
                uses: vec!["branch-db".to_owned()],
                drops: Vec::new(),
                ..Default::default()
            },
        )
        .expect("spawn b");
    let rendered = BRANCH_ENV.replace("54322", &port(&manager, "b").to_string());
    assert_eq!(
        std::fs::read_to_string(spawned.branch.workspace_path.join(".env.db")).expect("read"),
        rendered
    );

    let spawned = manager.spawn("c", None).expect("spawn c");
    let workspace = spawned.branch.workspace_path.clone();
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.db")).expect("read"),
        SHARED_ENV
    );
    assert!(
        !spawned.branch.trackers.contains_key("branch-db"),
        "dropped at spawn, so never bound here"
    );

    let change = manager.use_tracker("c", "branch-db").expect("use");
    assert_eq!(change.replaced, ["shared-db"]);
    assert_eq!(change.started, ["fakedb"]);
    assert_eq!(change.files, 1);
    assert!(change.seeded, "the file came from branch-db's main");

    let rendered = BRANCH_ENV.replace("54322", &port(&manager, "c").to_string());
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.db")).expect("read"),
        rendered,
        "branch-db's main, with fakedb's port rendered in"
    );
    let branch = manager.store().find_branch("c").expect("instance");
    assert_eq!(branch.resources["fakedb"].status, ResourceStatus::Ready);
    // The binding is the one a spawn records: its rev is the lane's main.
    assert_eq!(
        branch.trackers["branch-db"].content_rev,
        manager.store().find_branch("b").expect("b").trackers["branch-db"].content_rev,
    );
    assert!(branch.trackers["branch-db"].content_rev.is_some());

    // And it round-trips: swapping back and forth restores each from the
    // rev the instance now holds for it.
    let back = manager.use_tracker("c", "shared-db").expect("use shared");
    assert!(!back.seeded, "shared-db has a rev of its own here");
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.db")).expect("read"),
        SHARED_ENV
    );
    let again = manager.use_tracker("c", "branch-db").expect("use again");
    assert!(!again.seeded, "branch-db has a rev of its own here now");
    assert_eq!(again.started, ["fakedb"]);
    let rendered = BRANCH_ENV.replace("54322", &port(&manager, "c").to_string());
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.db")).expect("read"),
        rendered
    );
}

#[test]
fn using_a_tracker_whose_main_is_empty_binds_it_empty() {
    let (_guard, temp) = tempdir();
    let manager = project(&temp, false);
    let spawned = manager.spawn("c", None).expect("spawn c");
    let workspace = spawned.branch.workspace_path.clone();
    assert!(!workspace.join(".env.db").exists());

    let change = manager.use_tracker("c", "branch-db").expect("use");
    assert_eq!(change.files, 0);
    assert!(!change.seeded);
    assert!(!workspace.join(".env.db").exists());
    let branch = manager.store().find_branch("c").expect("instance");
    assert_eq!(
        branch.trackers["branch-db"].content_rev, None,
        "nothing to bind, as at spawn"
    );
}

/// A tracker bound only on `--use` (negative priority, no role) whose paths
/// already hold a hand-written file: `use` brings its main, but captures
/// the file to the instance's branch first, so nothing is lost.
#[test]
fn using_a_tracker_keeps_what_was_already_at_its_paths() {
    let (_guard, temp) = tempdir();
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join(".gitignore"), "/.env.local\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    std::fs::write(
        store.paths().trackers.join("local-env.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.local\"]\npriority = -1\n",
    )
    .expect("write");
    let manager = BranchManager::open(store).expect("manager");
    std::fs::write(repo.join(".env.local"), "FROM_MAIN=1\n").expect("write");
    manager.seed_tracker_from_store("local-env").expect("seed");
    std::fs::remove_file(repo.join(".env.local")).expect("remove");

    let spawned = manager.spawn("c", None).expect("spawn c");
    assert!(!spawned.branch.trackers.contains_key("local-env"));
    let env = spawned.branch.workspace_path.join(".env.local");
    std::fs::write(&env, "BY_HAND=1\n").expect("write");

    let change = manager.use_tracker("c", "local-env").expect("use");
    assert!(change.seeded);
    assert_eq!(
        std::fs::read_to_string(&env).expect("read"),
        "FROM_MAIN=1\n"
    );

    let (lanes, branch) = manager
        .tracker_log_target("local-env", Some("c"))
        .expect("log target");
    let shown = Command::new("git")
        .arg("-C")
        .arg(lanes.as_str())
        .args(["show", &format!("{branch}:.env.local")])
        .output()
        .expect("git runs");
    assert_eq!(
        String::from_utf8_lossy(&shown.stdout),
        "BY_HAND=1\n",
        "the hand-written file is on c's branch of the tracker"
    );
}
