//! A capture into a secret tracker's lane keeps this instance's rendered
//! values out of it, as `capture_tracker` and `checkout_tracker` do. These
//! pin the two captures that did not: a checkpoint's, and the safety capture
//! `tracker pull` takes before it re-binds the lane head.
//!
//! Both captured straight from the workspace. The lane is shared by every
//! instance, so a checkpoint wrote this instance's allocated port into it,
//! and recorded that rev as the file's committed content: the re-render
//! after an undo no longer found the default it substitutes, and once the
//! rev was merged, the next instance's copy held this one's port and failed
//! to render.
//!
//! With committed values in the lane, a restore leaves the file holding
//! exactly its committed content, and the re-render after it must not take
//! that for a hand edit.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, UndoOptions};
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;
use newgit_core::tracker_repo::TrackerRepo;
use newgit_core::{NewgitError, SourceSubstrate};

/// The committed default every instance should see in the lane.
const COMMITTED_ENV: &str = "SUPABASE_URL=http://127.0.0.1:54321\n";

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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    store.write_resource_file(name, contents).expect("resource");
}

/// `render.rs`'s tracker-owned render: a secret `.env.local` holding the
/// committed default, rewritten to this instance's port.
const ENV_RESOURCE: &str = r#"ownership = "branch"
depends_on = ["runtime-env"]

[ports]
api = { start = 54400 }

[[render]]
path = ".env.local"
replace = [
  { find = "SUPABASE_URL=http://127.0.0.1:54321",
    with = "SUPABASE_URL=http://127.0.0.1:{{ports.api}}" },
]
"#;

fn setup_env_tracker(temp: &Utf8Path) -> MetadataStore {
    let store = setup(temp);
    std::fs::write(store.paths().project_root.join(".env.local"), COMMITTED_ENV)
        .expect("write env");
    write_resource(&store, "supabase", ENV_RESOURCE);

    let m = BranchManager::open(store).expect("manager");
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create tracker");
    m.track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    m.seed_tracker_from_store("runtime-env").expect("seed");
    MetadataStore::at(m.store().paths().project_root.clone())
}

fn lane(m: &BranchManager) -> TrackerRepo {
    let paths = m.store().paths();
    TrackerRepo::new(
        &paths.tracker_store,
        &paths.tracker_checkouts,
        "runtime-env",
    )
}

/// `.env.local` as the lane holds it at `rev`.
fn in_lane(m: &BranchManager, rev: &str) -> String {
    lane(m)
        .read_file(rev, Utf8Path::new(".env.local"))
        .expect("read lane")
        .expect("lane content")
}

fn read(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).expect("read")
}

#[test]
fn a_checkpoint_keeps_this_instances_ports_out_of_a_secret_tracker_lane() {
    let (_guard, temp) = tempdir();
    let store = setup_env_tracker(&temp);
    let m = BranchManager::open(store).expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let env = spawned.branch.workspace_path.join(".env.local");
    let api = spawned.branch.resources["supabase"].resolved_ports["api"];
    let rendered = format!("SUPABASE_URL=http://127.0.0.1:{api}\n");
    // The render did its job: the instance runs on its own port.
    assert_ne!(api, 54321);
    assert_eq!(read(&env), rendered);

    let checkpoint = m
        .checkpoint("feature-a", Some("before"))
        .expect("checkpoint");
    let rev = checkpoint
        .record
        .tracker_states
        .iter()
        .find(|state| state.name == "runtime-env")
        .and_then(|state| state.content_rev.clone())
        .expect("the checkpoint captured runtime-env");
    let captured = in_lane(&m, &rev);
    // The capture puts the file back: the instance is still running on it.
    let after_checkpoint = read(&env);

    // Undo restores the rev the checkpoint recorded and re-renders this
    // instance's values on top of it. Collected before asserting, so a
    // failure shows both halves.
    let undone = m.undo("feature-a", &UndoOptions::default()).expect("undo");
    let about_env: Vec<&String> = undone
        .warnings
        .iter()
        .filter(|warning| warning.contains(".env.local"))
        .collect();

    assert_eq!(
        captured, COMMITTED_ENV,
        "checkpoint {} captured this instance's port {api} into the shared `runtime-env` lane \
         (rev {rev}); the undo to it then warned: {about_env:#?}",
        checkpoint.record.id
    );
    assert_eq!(
        after_checkpoint, rendered,
        "the checkpoint left it unrendered"
    );
    // Nor is the restored file, which holds exactly the committed content,
    // taken for a hand edit by the re-render.
    assert!(
        about_env.is_empty(),
        "undo could not put this instance's values back cleanly: {about_env:#?}"
    );
    assert_eq!(read(&env), rendered);
}

/// What a shared lane means: a checkpointed rev, once merged, is what the
/// next instance starts from. A key added beside the rendered value travels
/// with it; this instance's port must not.
#[test]
fn a_merged_checkpoint_hands_the_next_instance_the_edit_but_not_the_port() {
    let (_guard, temp) = tempdir();
    let store = setup_env_tracker(&temp);
    let m = BranchManager::open(store).expect("manager");

    let a = m.spawn("feature-a", None).expect("spawn a");
    let a_api = a.branch.resources["supabase"].resolved_ports["api"];
    std::fs::write(
        a.branch.workspace_path.join(".env.local"),
        format!("SUPABASE_URL=http://127.0.0.1:{a_api}\nSTRIPE_KEY=sk_test_123\n"),
    )
    .expect("edit");
    m.checkpoint("feature-a", Some("stripe"))
        .expect("checkpoint");
    m.merge_tracker("feature-a", "runtime-env").expect("merge");

    let b = m.spawn("feature-b", None).expect("spawn b");
    let b_api = b.branch.resources["supabase"].resolved_ports["api"];
    assert_ne!(a_api, b_api);
    assert_eq!(
        read(&b.branch.workspace_path.join(".env.local")),
        format!("SUPABASE_URL=http://127.0.0.1:{b_api}\nSTRIPE_KEY=sk_test_123\n"),
        "`feature-b` did not get its own port"
    );
    assert!(b.is_complete(), "{:#?}", b.resources);
}

/// `tracker pull` captures what the instance holds before re-binding the
/// lane head over it, which makes that capture the one copy of an edit made
/// since the last capture. Checking it out has to bring the edit back on
/// this instance's port.
#[test]
fn a_pull_keeps_this_instances_ports_out_of_its_safety_capture() {
    let (_guard, temp) = tempdir();
    let store = setup_env_tracker(&temp);
    let m = BranchManager::open(store).expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let env = spawned.branch.workspace_path.join(".env.local");
    let api = spawned.branch.resources["supabase"].resolved_ports["api"];
    let edited = format!("SUPABASE_URL=http://127.0.0.1:{api}\nSTRIPE_KEY=sk_test_123\n");
    std::fs::write(&env, &edited).expect("edit");

    let pulled = m.pull_tracker("feature-a", "runtime-env").expect("pull");
    // The lane head, rendered: the edit is not in it.
    assert_eq!(read(&env), format!("SUPABASE_URL=http://127.0.0.1:{api}\n"));

    // The safety capture is the tip of this instance's branch in the lane,
    // named by the pin `tracker checkout --rev` looks up.
    let (store, branch) = m
        .tracker_log_target("runtime-env", Some("feature-a"))
        .expect("log target");
    let tip = git(&store, &["rev-parse", &branch]);
    let safety = git(
        &store,
        &[
            "for-each-ref",
            "--points-at",
            tip.trim(),
            "--format=%(refname:lstrip=4)",
            "refs/trackers/runtime-env/pins/",
        ],
    )
    .trim()
    .to_owned();
    assert_eq!(
        in_lane(&m, &safety),
        "SUPABASE_URL=http://127.0.0.1:54321\nSTRIPE_KEY=sk_test_123\n",
        "the pull's safety capture (rev {safety}) holds this instance's port {api}"
    );

    let restored = m
        .checkout_tracker("feature-a", "runtime-env", Some(&safety))
        .expect("checkout");
    assert_eq!(read(&env), edited);
    // Neither restore is taken for a hand edit, and both re-render.
    assert!(pulled.warnings.is_empty(), "{:#?}", pulled.warnings);
    assert!(restored.warnings.is_empty(), "{:#?}", restored.warnings);
}

/// The file goes back even when the capture fails: the instance is still
/// running against its rendered values. The failure is a bound rev the lane
/// no longer pins, which the capture cannot parent onto.
#[test]
fn a_failed_capture_leaves_the_rendered_file_as_it_was() {
    let (_guard, temp) = tempdir();
    let store = setup_env_tracker(&temp);
    let m = BranchManager::open(store).expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let env = spawned.branch.workspace_path.join(".env.local");
    let rendered = read(&env);
    let bound = spawned.branch.trackers["runtime-env"]
        .content_rev
        .clone()
        .expect("bound rev");
    lane(&m).forget(&bound).expect("forget");

    let checkpoint = m.checkpoint("feature-a", None);
    assert!(
        matches!(checkpoint, Err(NewgitError::NoSnapshot { .. })),
        "{checkpoint:?}"
    );
    assert_eq!(
        read(&env),
        rendered,
        "the failed checkpoint left it unrendered"
    );

    let pulled = m.pull_tracker("feature-a", "runtime-env");
    assert!(
        matches!(pulled, Err(NewgitError::NoSnapshot { .. })),
        "{pulled:?}"
    );
    assert_eq!(read(&env), rendered, "the failed pull left it unrendered");
}
