use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BindOrigin, BranchManager};
use newgit_core::store::MetadataStore;
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
fn tracker_add_writes_definition_and_gitignore() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let m = manager(store);

    let outcome = m.add_tracker("runtime-env", "env-file").expect("add");
    assert!(outcome.path.is_file());
    assert_eq!(outcome.ignored_patterns, vec!["/.env.local".to_owned()]);

    let gitignore = std::fs::read_to_string(repo.join(".gitignore")).expect("gitignore");
    assert!(gitignore.contains("# newgit tracker: runtime-env"));
    assert!(gitignore.contains("/.env.local"));

    // Re-opening validates the definition and reports no gitignore warnings.
    let m = manager(MetadataStore::at(repo));
    assert_eq!(m.tracker_definitions().len(), 1);
    assert!(m.gitignore_warnings().is_empty());

    let duplicate = m.add_tracker("runtime-env", "env-file");
    assert!(matches!(duplicate, Err(NewgitError::AlreadyExists(_))));
    let unknown = m.add_tracker("x", "no-such-template");
    assert!(matches!(unknown, Err(NewgitError::UnknownTemplate(_))));
}

#[test]
fn spawn_materializes_env_file_per_instance() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    manager(store)
        .add_tracker("runtime-env", "env-file")
        .expect("add");

    let m = manager(MetadataStore::at(repo));
    let outcome = m.spawn("feature-a", None).expect("spawn");

    // Content materialized from the committed template, bound with a rev.
    let env = outcome.branch.workspace_path.join(".env.local");
    assert!(env.is_file());
    let bind = &outcome.trackers[0];
    assert_eq!(bind.origin, BindOrigin::Template);
    let rev = bind.content_rev.clone().expect("baseline rev");
    assert_eq!(
        outcome.branch.trackers["runtime-env"]
            .content_rev
            .as_deref(),
        Some(rev.as_str())
    );

    // Pinned: a second instance gets its own fresh copy, same content rev
    // (content-addressed dedupe).
    let second = m.spawn("feature-b", None).expect("spawn b");
    assert_eq!(second.trackers[0].content_rev, Some(rev));
}

#[test]
fn capture_and_restore_roundtrip_with_safety_capture() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    manager(store)
        .add_tracker("runtime-env", "env-file")
        .expect("add");
    let m = manager(MetadataStore::at(repo));
    let spawned = m.spawn("feature-a", None).expect("spawn");
    let env = spawned.branch.workspace_path.join(".env.local");
    let baseline = spawned.trackers[0].content_rev.clone().expect("baseline");

    // Edit and capture: new rev, binding updated.
    std::fs::write(&env, "API_KEY=abc\n").expect("edit");
    let captured = m
        .capture_tracker("feature-a", "runtime-env")
        .expect("capture");
    assert!(captured.changed);
    assert_ne!(captured.rev, baseline);

    // Recapture without changes dedupes.
    let again = m
        .capture_tracker("feature-a", "runtime-env")
        .expect("recapture");
    assert!(!again.changed);
    assert_eq!(again.rev, captured.rev);

    // Diverge, then restore to the captured rev: divergence is auto-saved.
    std::fs::write(&env, "API_KEY=oops\n").expect("diverge");
    let restored = m
        .restore_tracker("feature-a", "runtime-env", None)
        .expect("restore");
    assert_eq!(restored.rev, captured.rev);
    let safety = restored.safety_rev.expect("divergence saved");
    assert_eq!(
        std::fs::read_to_string(&env).expect("read"),
        "API_KEY=abc\n"
    );

    // The safety rev is itself restorable.
    let back = m
        .restore_tracker("feature-a", "runtime-env", Some(&safety))
        .expect("restore safety");
    assert_eq!(back.rev, safety);
    assert_eq!(
        std::fs::read_to_string(&env).expect("read"),
        "API_KEY=oops\n"
    );
}

#[test]
fn rebase_propagation_flows_to_new_spawns_and_flags_behind() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    // A rebase file-snapshot tracker owning a config file.
    std::fs::write(
        store.paths().trackers.join("shared-config.toml"),
        r#"kind = "file-snapshot"
audience = "project-devs"
storage = "local"
propagation = "rebase"
paths = ["config/shared.json"]
"#,
    )
    .expect("write def");
    std::fs::write(repo.join(".gitignore"), "/config/shared.json\n").expect("gitignore");

    let m = manager(MetadataStore::at(repo.clone()));
    let a = m.spawn("feature-a", None).expect("spawn a");
    // No lane head and no template: bound without content.
    assert_eq!(a.trackers[0].origin, BindOrigin::Nothing);

    // A creates content and captures; the lane head moves.
    let config_a = a.branch.workspace_path.join("config/shared.json");
    std::fs::create_dir_all(config_a.parent().expect("has parent")).expect("mkdir");
    std::fs::write(&config_a, "{\"v\":1}\n").expect("write");
    let captured = m
        .capture_tracker("feature-a", "shared-config")
        .expect("capture");

    // A new spawn materializes from the lane head.
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

    // A moves the lane head again; B is now behind, and materialize pulls.
    std::fs::write(&config_a, "{\"v\":2}\n").expect("write v2");
    m.capture_tracker("feature-a", "shared-config")
        .expect("capture v2");
    let statuses = m.statuses().expect("statuses");
    let b_report = statuses
        .iter()
        .find(|r| r.branch.name == "feature-b")
        .expect("b");
    assert!(b_report.trackers[0].behind);

    m.materialize_tracker("feature-b", "shared-config")
        .expect("pull");
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
    assert!(!b_report.trackers[0].behind);
}

#[test]
fn disjoint_lanes_and_manual_materialize_are_enforced() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    for (name, path) in [("a", "src/generated"), ("b", "src/generated/sdk")] {
        std::fs::write(
            store.paths().trackers.join(format!("{name}.toml")),
            format!(
                "kind = \"file-snapshot\"\naudience = \"project-devs\"\nstorage = \"local\"\npropagation = \"manual\"\npaths = [\"{path}\"]\n"
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
        m.materialize_tracker("feature-a", "a"),
        Err(NewgitError::Unsupported(_))
    ));
}
