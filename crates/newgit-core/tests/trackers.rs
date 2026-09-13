use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BindOrigin, BranchManager, UndoOptions};
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;
use newgit_core::{NewgitError, SourceSubstrate};

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

fn manager(store: MetadataStore) -> BranchManager {
    BranchManager::open(store).expect("manager")
}

#[test]
fn tracker_create_and_track_write_definition_and_gitignore() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);

    let outcome = m
        .create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");
    assert!(outcome.path.is_file());
    assert!(outcome.ignored_patterns.is_empty());

    let tracked = m
        .track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    assert_eq!(tracked.ignored_patterns, vec!["/.env.local".to_owned()]);

    let gitignore = std::fs::read_to_string(repo.join(".gitignore")).expect("gitignore");
    assert!(gitignore.contains("# newgit tracker: runtime-env"));
    assert!(gitignore.contains("/.env.local"));

    // Re-opening validates the definition and reports no gitignore warnings.
    let m = manager(MetadataStore::at(repo));
    assert_eq!(m.tracker_definitions().len(), 1);
    assert_eq!(
        m.tracker_definitions()[0].paths,
        vec![Utf8PathBuf::from(".env.local")]
    );
    assert!(!m.tracker_definitions()[0].merge_with_source);
    assert!(m.gitignore_warnings().is_empty());

    let duplicate = m.create_tracker("runtime-env", "user", Storage::Local, false);
    assert!(matches!(duplicate, Err(NewgitError::AlreadyExists(_))));
}

#[test]
fn captured_lane_stays_branch_local_until_merged() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");
    m.track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");

    let m = manager(MetadataStore::at(repo));
    let first = m.spawn("feature-a", None).expect("spawn");
    assert_eq!(first.trackers[0].origin, BindOrigin::Nothing);
    assert!(!first.branch.workspace_path.join(".env.local").exists());

    let env = first.branch.workspace_path.join(".env.local");
    std::fs::write(&env, "API_KEY=abc\n").expect("write env");
    let captured = m
        .capture_tracker("feature-a", "runtime-env")
        .expect("capture");
    assert_eq!(
        m.store().find_branch("feature-a").expect("reload").trackers["runtime-env"]
            .content_rev
            .as_deref(),
        Some(captured.rev.as_str())
    );

    // Capture alone does not move the lane head/default.
    let second = m.spawn("feature-b", None).expect("spawn b");
    assert_eq!(second.trackers[0].origin, BindOrigin::Nothing);
    assert!(!second.branch.workspace_path.join(".env.local").exists());

    // Merging the tracker promotes this branch-local rev to the default for
    // future branches and explicit pulls.
    let merged = m
        .merge_tracker("feature-a", "runtime-env")
        .expect("merge tracker");
    assert_eq!(merged.rev, captured.rev);

    let third = m.spawn("feature-c", None).expect("spawn c");
    assert_eq!(third.trackers[0].origin, BindOrigin::LaneHead);
    assert_eq!(third.trackers[0].content_rev, Some(merged.rev));
    assert_eq!(
        std::fs::read_to_string(third.branch.workspace_path.join(".env.local")).expect("read"),
        "API_KEY=abc\n"
    );
}

#[test]
fn capture_and_restore_roundtrip_with_safety_capture() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");
    m.track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    let m = manager(MetadataStore::at(repo));
    let spawned = m.spawn("feature-a", None).expect("spawn");
    let env = spawned.branch.workspace_path.join(".env.local");

    // Edit and capture: new rev, binding updated.
    std::fs::write(&env, "API_KEY=abc\n").expect("edit");
    let captured = m
        .capture_tracker("feature-a", "runtime-env")
        .expect("capture");
    assert!(captured.changed);

    // Recapture without changes dedupes.
    let again = m
        .capture_tracker("feature-a", "runtime-env")
        .expect("recapture");
    assert!(!again.changed);
    assert_eq!(again.rev, captured.rev);

    // Diverge, then check out the captured rev: divergence is auto-saved.
    std::fs::write(&env, "API_KEY=oops\n").expect("diverge");
    let restored = m
        .checkout_tracker("feature-a", "runtime-env", None)
        .expect("checkout");
    assert_eq!(restored.rev, captured.rev);
    let safety = restored.safety_rev.expect("divergence saved");
    assert_eq!(
        std::fs::read_to_string(&env).expect("read"),
        "API_KEY=abc\n"
    );

    // The safety rev is itself available for checkout.
    let back = m
        .checkout_tracker("feature-a", "runtime-env", Some(&safety))
        .expect("checkout safety");
    assert_eq!(back.rev, safety);
    assert_eq!(
        std::fs::read_to_string(&env).expect("read"),
        "API_KEY=oops\n"
    );
}

#[test]
fn merge_and_pull_flow_to_new_and_existing_instances() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    // A tracker owning a config file that should travel with source merges.
    std::fs::write(
        store.paths().trackers.join("shared-config.toml"),
        r#"audience = "project-devs"
storage = "local"
merge_with_source = true
paths = ["config/shared.json"]
"#,
    )
    .expect("write def");
    std::fs::write(repo.join(".gitignore"), "/config/shared.json\n").expect("gitignore");

    let m = manager(MetadataStore::at(repo.clone()));
    let a = m.spawn("feature-a", None).expect("spawn a");
    // No lane head and no template: bound without content.
    assert_eq!(a.trackers[0].origin, BindOrigin::Nothing);

    assert!(m.tracker_definitions()[0].merge_with_source);

    // A creates content and captures; the lane head does not move until merge.
    let config_a = a.branch.workspace_path.join("config/shared.json");
    std::fs::create_dir_all(config_a.parent().expect("has parent")).expect("mkdir");
    std::fs::write(&config_a, "{\"v\":1}\n").expect("write");
    let captured = m
        .capture_tracker("feature-a", "shared-config")
        .expect("capture");

    let b_empty = m
        .spawn("feature-b-empty", None)
        .expect("spawn b before merge");
    assert_eq!(b_empty.trackers[0].origin, BindOrigin::Nothing);

    let merged = m
        .merge_tracker("feature-a", "shared-config")
        .expect("merge tracker");
    assert_eq!(merged.rev, captured.rev);

    // A new spawn projects from the lane head.
    let b = m.spawn("feature-b", None).expect("spawn b");
    assert_eq!(b.trackers[0].origin, BindOrigin::LaneHead);
    assert_eq!(
        b.trackers[0].content_rev.as_deref(),
        Some(captured.rev.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(b.branch.workspace_path.join("config/shared.json")).expect("read"),
        "{\"v\":1}\n"
    );

    // A captures and merges again; B is now behind, and pull catches up.
    std::fs::write(&config_a, "{\"v\":2}\n").expect("write v2");
    m.capture_tracker("feature-a", "shared-config")
        .expect("capture v2");
    m.merge_tracker("feature-a", "shared-config")
        .expect("merge v2");
    let statuses = m.statuses().expect("statuses");
    let b_report = statuses
        .iter()
        .find(|r| r.branch.name == "feature-b")
        .expect("b");
    assert!(b_report.trackers[0].never_pulled() || b_report.trackers[0].diverged());

    m.pull_tracker("feature-b", "shared-config").expect("pull");
    assert_eq!(
        std::fs::read_to_string(b_report.branch.workspace_path.join("config/shared.json"))
            .expect("read"),
        "{\"v\":2}\n"
    );
    let statuses = m.statuses().expect("statuses");
    let b_report = statuses
        .iter()
        .find(|r| r.branch.name == "feature-b")
        .expect("b");
    assert!(!b_report.trackers[0].never_pulled() && !b_report.trackers[0].diverged());
}

#[test]
fn workspace_marker_is_invisible_to_git() {
    let (_guard, temp) = tempdir();
    let m = manager(setup(&temp));
    let outcome = m.spawn("feature-a", None).expect("spawn");

    let status = Command::new("git")
        .args([
            "-C",
            outcome.branch.workspace_path.as_str(),
            "status",
            "--porcelain",
        ])
        .output()
        .expect("git status");
    assert!(
        status.stdout.is_empty(),
        "workspace not clean: {}",
        String::from_utf8_lossy(&status.stdout)
    );
}

#[test]
fn dual_tracked_paths_warn_loudly() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    // README.md is committed; a tracker owning it is dual-tracked.
    std::fs::write(
        store.paths().trackers.join("readme.toml"),
        "audience = \"project-devs\"\nstorage = \"local\"\nmerge_with_source = true\npaths = [\"README.md\"]\n",
    )
    .expect("write def");

    let m = manager(MetadataStore::at(repo));
    let warnings = m.gitignore_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("dual-tracked"), "{}", warnings[0]);
}

#[test]
fn disjoint_lanes_and_empty_pull_are_enforced() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    for (name, path) in [("a", "src/generated"), ("b", "src/generated/sdk")] {
        std::fs::write(
            store.paths().trackers.join(format!("{name}.toml")),
            format!(
                "audience = \"project-devs\"\nstorage = \"local\"\nmerge_with_source = false\npaths = [\"{path}\"]\n"
            ),
        )
        .expect("write def");
    }
    assert!(matches!(
        BranchManager::open(MetadataStore::at(repo.clone())),
        Err(NewgitError::TrackerPathConflict { .. })
    ));

    std::fs::remove_file(store.paths().trackers.join("b.toml")).expect("rm");
    let m = manager(MetadataStore::at(repo));
    m.spawn("feature-a", None).expect("spawn");
    assert!(matches!(
        m.pull_tracker("feature-a", "a"),
        Err(NewgitError::Unsupported(_))
    ));
}

/// M6's SQLite case. The claim is that it needs no special support: a
/// database that is just a file on disk is a tracker, and binary content has
/// to survive capture and restore byte-for-byte.
#[test]
fn a_binary_database_file_round_trips_through_a_plain_file_tracker() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);
    m.create_tracker("dev-db", "project-devs", Storage::Local, false)
        .expect("create");
    m.track_paths("dev-db", &[Utf8PathBuf::from("data/dev.sqlite")])
        .expect("track");

    let m = manager(MetadataStore::at(repo));
    let spawned = m.spawn("feature-a", None).expect("spawn");
    let db = spawned.branch.workspace_path.join("data/dev.sqlite");
    std::fs::create_dir_all(db.parent().expect("parent")).expect("mkdir");

    // A SQLite header, a NUL-heavy page, and a high byte: nothing here
    // survives being treated as text.
    let mut original = b"SQLite format 3\0".to_vec();
    original.extend(std::iter::repeat_n(0u8, 200));
    original.extend([0xff, 0x00, 0x80, b'r', b'o', b'w']);
    std::fs::write(&db, &original).expect("write db");

    m.capture_tracker("feature-a", "dev-db").expect("capture");
    let checkpoint = m
        .checkpoint("feature-a", Some("seeded"))
        .expect("checkpoint");

    // The agent corrupts the database, as agents do.
    std::fs::write(&db, b"truncated garbage").expect("clobber");
    m.undo(
        "feature-a",
        &UndoOptions {
            to: Some(checkpoint.record.id.clone()),
            ..Default::default()
        },
    )
    .expect("undo");

    assert_eq!(
        std::fs::read(&db).expect("read db"),
        original,
        "a binary database must come back byte-for-byte"
    );

    // And a later instance projects the merged lane content the same way.
    m.merge_tracker("feature-a", "dev-db").expect("merge");
    let later = m.spawn("feature-b", None).expect("spawn b");
    assert_eq!(
        std::fs::read(later.branch.workspace_path.join("data/dev.sqlite")).expect("read"),
        original
    );
}

/// Tracker-owned content must be invisible to the workspace's Git even when
/// the store's `.gitignore` edit has not been committed yet — otherwise an
/// agent running `git add -A` commits a lane's content into source history,
/// which is exactly what audience is supposed to prevent by construction.
#[test]
fn tracker_paths_are_ignored_in_a_workspace_before_gitignore_is_committed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");
    m.track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    m.create_tracker("generated-sdk", "public", Storage::Local, false)
        .expect("create");
    m.track_paths("generated-sdk", &[Utf8PathBuf::from("src/generated")])
        .expect("track");

    // Deliberately do NOT commit the store's .gitignore: this is the state a
    // user is in immediately after `newgit tracker track`.
    let m = manager(MetadataStore::at(repo));
    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();

    std::fs::write(workspace.join(".env.local"), "SECRET=hunter2\n").expect("write");
    std::fs::create_dir_all(workspace.join("src/generated")).expect("mkdir");
    std::fs::write(workspace.join("src/generated/api.ts"), "export {};\n").expect("write");

    let status = Command::new("git")
        .args(["-C", workspace.as_str(), "status", "--porcelain"])
        .output()
        .expect("git status");
    let porcelain = String::from_utf8_lossy(&status.stdout);
    assert!(
        porcelain.trim().is_empty(),
        "tracker-owned content must not appear in git status, got: {porcelain}"
    );

    // The strong form: even `git add -A` cannot pick it up.
    git(&workspace, &["add", "-A"]);
    let staged = Command::new("git")
        .args(["-C", workspace.as_str(), "diff", "--cached", "--name-only"])
        .output()
        .expect("git diff");
    assert!(
        String::from_utf8_lossy(&staged.stdout).trim().is_empty(),
        "a lane's content must not be stageable into source history"
    );
}

/// A lane starts empty and `capture` reads from an instance workspace, so the
/// first spawn of an env-carrying tracker used to come up without its files:
/// you had to spawn, `cp` the content in, capture, merge, and re-run prepare.
/// Seeding from the store repo makes the first spawn work instead.
#[test]
fn a_lane_seeded_from_the_store_repo_reaches_the_first_instance() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    // The content an adopting project already has on disk.
    std::fs::create_dir_all(repo.join("packages/db")).expect("mkdir");
    std::fs::write(repo.join("packages/db/.env"), "DB=local\n").expect("write");
    std::fs::write(repo.join(".env.local"), "API=local\n").expect("write");

    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");
    let tracked = m
        .track_paths(
            "runtime-env",
            &[
                Utf8PathBuf::from("packages/db/.env"),
                Utf8PathBuf::from(".env.local"),
            ],
        )
        .expect("track");
    assert!(
        tracked.seedable,
        "content is on disk, so the lane can be seeded without an instance"
    );

    let seeded = m.seed_tracker_from_store("runtime-env").expect("seed");
    assert_eq!(seeded.files, 2);
    assert!(seeded.changed);
    assert!(seeded.missing_paths.is_empty());

    // The whole point: the first instance comes up with the content.
    // Reopened because each CLI invocation loads definitions fresh.
    let m = manager(MetadataStore::at(repo));
    let spawned = m.spawn("bootstrap", None).expect("spawn");
    let workspace = &spawned.branch.workspace_path;
    assert_eq!(
        std::fs::read_to_string(workspace.join("packages/db/.env")).expect("read"),
        "DB=local\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("read"),
        "API=local\n"
    );
    assert_eq!(
        spawned.branch.trackers["runtime-env"]
            .content_rev
            .as_deref(),
        Some(seeded.rev.as_str())
    );

    // Re-seeding unchanged content is a no-op against the lane head.
    let again = m.seed_tracker_from_store("runtime-env").expect("reseed");
    assert_eq!(again.rev, seeded.rev);
    assert!(!again.changed);
}

/// Seeding reports declared paths with nothing behind them rather than
/// quietly capturing a partial lane, and refuses outright when none exist.
#[test]
fn seeding_reports_paths_that_are_not_on_disk() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);

    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");
    let tracked = m
        .track_paths(
            "runtime-env",
            &[
                Utf8PathBuf::from(".env.local"),
                Utf8PathBuf::from(".env.ci"),
            ],
        )
        .expect("track");
    assert!(
        !tracked.seedable,
        "nothing on disk yet, so do not advertise seeding"
    );

    assert!(matches!(
        m.seed_tracker_from_store("runtime-env"),
        Err(NewgitError::NothingToSeed { .. })
    ));

    std::fs::write(repo.join(".env.local"), "API=local\n").expect("write");
    let seeded = m.seed_tracker_from_store("runtime-env").expect("seed");
    assert_eq!(seeded.files, 1);
    assert_eq!(seeded.missing_paths, vec![Utf8PathBuf::from(".env.ci")]);
}

#[test]
fn remove_tracker_refuses_the_default_source_tracker() {
    let (_guard, temp) = tempdir();
    let m = manager(setup(&temp));
    assert!(matches!(
        m.remove_tracker("source"),
        Err(NewgitError::Unsupported(_))
    ));
}

#[test]
fn remove_tracker_names_the_valid_ones_when_asked_for_an_unknown_name() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");

    // Definitions load at `BranchManager::open`, like every CLI invocation.
    let m = manager(MetadataStore::at(repo));
    let err = m.remove_tracker("nope").expect_err("unknown tracker");
    let message = err.to_string();
    assert!(message.contains("runtime-env"), "{message}");
}

#[test]
fn remove_tracker_refuses_while_a_live_instance_is_bound() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");
    m.track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");

    let m = manager(MetadataStore::at(repo));
    m.spawn("feature-a", None).expect("spawn");

    let err = m
        .remove_tracker("runtime-env")
        .expect_err("bound to a live instance");
    assert!(matches!(err, NewgitError::Unsupported(_)));
    assert!(err.to_string().contains("feature-a"));

    // Definition and content survive the refusal.
    assert_eq!(m.tracker_definitions().len(), 1);
}

/// `tracker remove` undoes exactly what `tracker create`/`tracker track` did
/// — the definition file and the store's `.gitignore` block — and leaves
/// captured content for `newgit cleanup` to reclaim once it is unreferenced.
#[test]
fn remove_tracker_reverses_the_definition_and_gitignore_and_leaves_snapshots_for_cleanup() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create");
    m.track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");

    let m = manager(MetadataStore::at(&repo));
    let spawned = m.spawn("feature-a", None).expect("spawn");
    std::fs::write(spawned.branch.workspace_path.join(".env.local"), "A=1\n").expect("write");
    let captured = m
        .capture_tracker("feature-a", "runtime-env")
        .expect("capture");
    m.merge_tracker("feature-a", "runtime-env").expect("merge");
    let snapshot_dir = m
        .store()
        .paths()
        .snapshots
        .join("runtime-env")
        .join(&captured.rev);
    assert!(snapshot_dir.is_dir(), "captured content exists on disk");

    // Not removable while the instance is still bound to it.
    assert!(m.remove_tracker("runtime-env").is_err());
    m.remove("feature-a", &repo, ArchivedCheckpoints::Keep)
        .expect("remove instance");

    let definition_path = m.tracker_definitions()[0].name.clone();
    assert_eq!(definition_path, "runtime-env");
    let outcome = m.remove_tracker("runtime-env").expect("remove tracker");
    assert!(!outcome.path.exists(), "definition file deleted");
    assert_eq!(outcome.gitignore_removed, vec!["/.env.local".to_owned()]);

    let gitignore = std::fs::read_to_string(repo.join(".gitignore")).expect("gitignore");
    assert!(
        !gitignore.contains("runtime-env"),
        "the labeled block is gone: {gitignore}"
    );

    let m = manager(MetadataStore::at(&repo));
    assert!(m.tracker_definitions().is_empty());

    // The captured content is untouched — it is `newgit cleanup`'s job, not
    // `tracker remove`'s, to reclaim it once nothing pins it.
    assert!(snapshot_dir.is_dir());
    let cleaned = m
        .cleanup(false, ArchivedCheckpoints::Keep)
        .expect("cleanup");
    assert_eq!(cleaned.pruned.len(), 1);
    assert_eq!(cleaned.pruned[0].tracker, "runtime-env");
    assert!(
        !snapshot_dir.is_dir(),
        "a removed tracker's head is no longer pinned once nothing else claims it"
    );
}
