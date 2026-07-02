use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BindOrigin, BranchManager};
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
