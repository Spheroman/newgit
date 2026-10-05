//! Tracker lanes are Git commits in the store's lanes repo
//! (`.newgit/local/lanes.git`), not snapshot directories. These tests pin the
//! properties that change is for: revs are content-addressed across
//! machines, lane content never enters the store repo or a workspace's Git,
//! and a pruned rev's content is actually gone.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::config::WorkspaceSection;
use newgit_core::lane::TrackerLane;
use newgit_core::manager::{BranchManager, UndoOptions};
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

const SECRET: &str = "STRIPE_KEY=sk_live_do_not_leak_4f1c\n";

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

fn setup(temp: &Utf8Path, name: &str) -> MetadataStore {
    let repo = temp.join(name);
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join(format!("{name}-workspaces"))),
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

/// A store with an `env` tracker owning `.env.local`, reopened so the
/// definition is loaded.
fn with_env_tracker(store: MetadataStore) -> BranchManager {
    let manager = BranchManager::open(store).expect("manager");
    manager
        .create_tracker("env", "user:jack", Storage::Local, false)
        .expect("create tracker");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    BranchManager::open(manager.store().clone()).expect("reopen")
}

fn lane(store: &MetadataStore, tracker: &str) -> TrackerLane {
    let paths = store.paths();
    TrackerLane::new(&paths.lanes, &paths.lane_checkouts, tracker)
}

/// Whether any object in a Git repository (reachable or not, loose or
/// packed) contains `needle`.
fn repo_contains(git_dir: &Utf8Path, needle: &str) -> bool {
    let objects = git(
        git_dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    objects
        .lines()
        .any(|object| git(git_dir, &["cat-file", "-p", object]).contains(needle))
}

#[test]
fn identical_content_is_the_same_rev_in_two_unrelated_stores() {
    let (_guard, temp) = tempdir();
    let laptop = with_env_tracker(setup(&temp, "laptop"));
    let desktop = with_env_tracker(setup(&temp, "desktop"));

    let mut revs = Vec::new();
    for manager in [&laptop, &desktop] {
        let spawned = manager.spawn("feature-a", None).expect("spawn");
        std::fs::write(spawned.branch.workspace_path.join(".env.local"), "A=1\n").expect("write");
        revs.push(
            manager
                .capture_tracker("feature-a", "env")
                .expect("capture")
                .rev,
        );
    }
    assert_eq!(
        revs[0], revs[1],
        "a rev names content, not who captured it or when — the remote relies on that"
    );

    // And within one lane, capturing the same content again is no change.
    let again = laptop
        .capture_tracker("feature-a", "env")
        .expect("capture again");
    assert_eq!(again.rev, revs[0]);
    assert!(!again.changed);
}

#[test]
fn lane_content_never_enters_the_store_repo_or_a_workspace() {
    let (_guard, temp) = tempdir();
    let manager = with_env_tracker(setup(&temp, "store"));
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    std::fs::write(workspace.join(".env.local"), SECRET).expect("write");
    manager
        .capture_tracker("feature-a", "env")
        .expect("capture");
    manager.merge_tracker("feature-a", "env").expect("merge");
    manager.checkpoint("feature-a", None).expect("checkpoint");

    // A second instance projects the lane head — and its clone is made after
    // the secret was captured, so a hardlinked object store would carry it.
    let second = manager.spawn("feature-b", None).expect("spawn b");
    assert_eq!(
        std::fs::read_to_string(second.branch.workspace_path.join(".env.local")).expect("read"),
        SECRET,
        "the lane head is projected into new instances"
    );

    let store_root = manager.store().paths().project_root.clone();
    assert!(
        git(&store_root, &["for-each-ref", "refs/lanes/"]).is_empty(),
        "no lane refs in the store repo, so `git push --mirror` cannot publish them"
    );
    for repo in [&store_root, &workspace, &second.branch.workspace_path] {
        assert!(
            !repo_contains(repo, "sk_live_do_not_leak"),
            "{repo} holds lane content in its object store"
        );
    }
    assert!(repo_contains(
        &manager.store().paths().lanes,
        "sk_live_do_not_leak"
    ));
}

#[test]
fn restore_reproduces_modes_and_absence_and_only_currently_owned_paths() {
    let (_guard, temp) = tempdir();
    let manager = BranchManager::open(setup(&temp, "store")).expect("manager");
    manager
        .create_tracker("tools", "project-devs", Storage::Local, false)
        .expect("create");
    manager
        .track_paths(
            "tools",
            &[Utf8PathBuf::from("bin"), Utf8PathBuf::from("notes.txt")],
        )
        .expect("track");
    let manager = BranchManager::open(manager.store().clone()).expect("reopen");
    let workspace = manager
        .spawn("feature-a", None)
        .expect("spawn")
        .branch
        .workspace_path;

    std::fs::create_dir_all(workspace.join("bin")).expect("mkdir");
    std::fs::write(workspace.join("bin/run.sh"), "#!/bin/sh\necho hi\n").expect("write");
    std::fs::write(workspace.join("bin/data.txt"), "data\n").expect("write");
    std::fs::write(workspace.join("notes.txt"), "notes\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            workspace.join("bin/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
    }
    let captured = manager
        .capture_tracker("feature-a", "tools")
        .expect("capture")
        .rev;

    // Mess up the workspace: delete one file, add one that was not captured.
    std::fs::remove_file(workspace.join("bin/data.txt")).expect("rm");
    std::fs::write(workspace.join("bin/extra.txt"), "extra\n").expect("write");
    let restored = manager
        .checkout_tracker("feature-a", "tools", Some(&captured))
        .expect("checkout");
    assert_eq!(restored.files, 3);
    assert_eq!(
        std::fs::read_to_string(workspace.join("bin/data.txt")).expect("read"),
        "data\n"
    );
    assert!(
        !workspace.join("bin/extra.txt").exists(),
        "restore reproduces absence"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(workspace.join("bin/run.sh"))
            .expect("stat")
            .permissions()
            .mode();
        assert_ne!(
            mode & 0o111,
            0,
            "the executable bit survives the round trip"
        );
    }

    // A rev only writes back paths the tracker owns *now*.
    let lane = lane(manager.store(), "tools");
    let definition = manager
        .tracker_definitions()
        .iter()
        .find(|definition| definition.name == "tools")
        .expect("tools");
    let mut narrowed = definition.clone();
    narrowed.paths = vec![Utf8PathBuf::from("bin")];
    std::fs::remove_file(workspace.join("notes.txt")).expect("rm");
    lane.restore(&workspace, &narrowed, &captured)
        .expect("restore narrowed");
    assert!(
        !workspace.join("notes.txt").exists(),
        "a path the definition no longer owns is not written back"
    );
}

#[test]
fn a_deposit_restores_after_the_checkout_cache_is_cleared() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, "store");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "echo dump-data > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "cp {{state_ref}} restored.sql"
"#,
    )
    .expect("write resource");
    let manager = BranchManager::open(store).expect("manager");
    manager
        .create_tracker("db-snapshots", "project-devs", Storage::Local, false)
        .expect("create tracker");
    let manager = BranchManager::open(manager.store().clone()).expect("reopen");
    let workspace = manager
        .spawn("feature-d", None)
        .expect("spawn")
        .branch
        .workspace_path;
    manager.checkpoint("feature-d", None).expect("checkpoint");

    // The checkout cache is disposable; undo must not depend on it.
    std::fs::remove_dir_all(&manager.store().paths().lane_checkouts).expect("clear cache");

    let undo = manager
        .undo("feature-d", &UndoOptions::default())
        .expect("undo");
    assert!(undo.resources[0].ok, "{:?}", undo.resources[0]);
    assert_eq!(
        std::fs::read_to_string(workspace.join("restored.sql")).expect("read"),
        "dump-data\n"
    );
}

#[test]
fn a_pruned_rev_is_gone_from_the_lanes_repo_not_just_unreferenced() {
    let (_guard, temp) = tempdir();
    let manager = with_env_tracker(setup(&temp, "store"));
    let spawned = manager.spawn("throwaway", None).expect("spawn");
    std::fs::write(spawned.branch.workspace_path.join(".env.local"), SECRET).expect("write");
    let rev = manager
        .capture_tracker("throwaway", "env")
        .expect("capture")
        .rev;
    manager
        .remove(
            "throwaway",
            &manager.store().paths().project_root.clone(),
            ArchivedCheckpoints::Purge,
        )
        .expect("remove");

    let outcome = manager
        .cleanup(false, ArchivedCheckpoints::Keep)
        .expect("cleanup");
    assert_eq!(
        outcome
            .pruned
            .iter()
            .map(|pruned| pruned.rev.as_str())
            .collect::<Vec<_>>(),
        [rev.as_str()]
    );
    let lanes = manager.store().paths().lanes.clone();
    assert!(!lane(manager.store(), "env").has_rev(&rev));
    assert!(
        !repo_contains(&lanes, "sk_live_do_not_leak"),
        "a pruned secret must not linger as an unreachable object"
    );
}

#[test]
fn capturing_thousands_of_files_does_not_stall() {
    let (_guard, temp) = tempdir();
    let manager = BranchManager::open(setup(&temp, "store")).expect("manager");
    manager
        .create_tracker("assets", "project-devs", Storage::Local, false)
        .expect("create");
    manager
        .track_paths("assets", &[Utf8PathBuf::from("assets")])
        .expect("track");
    let manager = BranchManager::open(manager.store().clone()).expect("reopen");
    let workspace = manager
        .spawn("feature-a", None)
        .expect("spawn")
        .branch
        .workspace_path;

    // Enough that Git's per-file output overflows a pipe buffer while newgit
    // is still writing the list of files in.
    let dir = workspace.join("assets/a-fairly-long-directory-name-to-fill-pipes-faster");
    std::fs::create_dir_all(&dir).expect("mkdir");
    for index in 0..3000 {
        std::fs::write(
            dir.join(format!("file-{index:05}.txt")),
            format!("{index}\n"),
        )
        .expect("write");
    }
    let captured = manager
        .capture_tracker("feature-a", "assets")
        .expect("capture");
    assert_eq!(captured.files, 3000);

    std::fs::remove_dir_all(workspace.join("assets")).expect("rm");
    let restored = manager
        .checkout_tracker("feature-a", "assets", Some(&captured.rev))
        .expect("checkout");
    assert_eq!(restored.files, 3000);
    assert_eq!(
        std::fs::read_to_string(dir.join("file-02999.txt")).expect("read"),
        "2999\n"
    );
}
