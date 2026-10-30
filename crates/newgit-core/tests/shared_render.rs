//! Several resources rendering one file (#245).
//!
//! An app's `.env.local` wants its own port and the database's URL. Each
//! value belongs to the resource it comes from, so each resource declares
//! its own `[[render]]` on the shared path. The file is one batch over every
//! claim on it, located in committed content, so it does not depend on
//! which resource bound first; capture, undo, drift, and unbinding all work
//! on the whole set of claims.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, Uncommitted, UndoOptions};
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;
use newgit_core::{NewgitError, SourceSubstrate};

const ENV: &str = "DATABASE_URL=postgres://127.0.0.1:54322/app\nPORT=3000\nDEBUG=1\n";

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

/// A store repo committing `.env.local` with its working defaults.
fn setup(temp: &Utf8Path, env: &str) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join(".env.local"), env).expect("write");
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

/// `db` owns the database URL in the app's env file.
const DB: &str = r#"ownership = "branch"

[ports]
db = { start = 54500 }

[[render]]
path = ".env.local"
replace = [
  { find = "DATABASE_URL=postgres://127.0.0.1:54322/app",
    with = "DATABASE_URL=postgres://127.0.0.1:{{ports.db}}/app" },
]
"#;

/// `web` owns its own port in the same file. No `depends_on`, so nothing
/// orders it against `db`: the file must not care.
const WEB: &str = r#"ownership = "branch"

[ports]
web = { start = 3100 }

[[render]]
path = ".env.local"
replace = [
  { find = "PORT=3000", with = "PORT={{ports.web}}" },
]
"#;

fn ports(m: &BranchManager, instance: &str) -> (u16, u16) {
    let branch = m.store().find_branch(instance).expect("instance");
    (
        branch.resources["db"].resolved_ports["db"],
        branch.resources["web"].resolved_ports["web"],
    )
}

fn rendered(db: u16, web: u16) -> String {
    format!("DATABASE_URL=postgres://127.0.0.1:{db}/app\nPORT={web}\nDEBUG=1\n")
}

#[test]
fn two_resources_render_their_own_values_into_one_file() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, ENV);
    write_resource(&store, "db", DB);
    write_resource(&store, "web", WEB);
    let m = BranchManager::open(store).expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    for outcome in &spawned.resources {
        assert!(outcome.render_error.is_none(), "{:?}", outcome.render_error);
    }
    let (db, web) = ports(&m, "feature-a");
    let workspace = spawned.branch.workspace_path.clone();
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("env"),
        rendered(db, web)
    );

    // Still invisible to Git.
    assert_eq!(git(&workspace, &["status", "--porcelain"]), "");
}

/// A dependent that renders into its dependency's file, the case from the
/// issue: the values meet in one file, each declared where it comes from.
#[test]
fn a_dependent_reaches_its_dependencys_export_in_a_shared_file() {
    let (_guard, temp) = tempdir();
    let store = setup(
        &temp,
        "DATABASE_URL=postgres://127.0.0.1:54322/app\nAPI_URL=http://127.0.0.1:54321\nPORT=3000\n",
    );
    write_resource(
        &store,
        "db",
        &format!("{DB}\n[exports]\nAPI_URL = \"http://127.0.0.1:{{{{ports.db}}}}/rest\"\n"),
    );
    write_resource(
        &store,
        "web",
        r#"ownership = "branch"
depends_on = ["db"]

[ports]
web = { start = 3100 }

[[render]]
path = ".env.local"
replace = [
  { find = "PORT=3000", with = "PORT={{ports.web}}" },
  { find = "API_URL=http://127.0.0.1:54321", with = "API_URL={{exports.API_URL}}" },
]
"#,
    );
    let m = BranchManager::open(store).expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let (db, web) = ports(&m, "feature-a");
    assert_eq!(
        std::fs::read_to_string(spawned.branch.workspace_path.join(".env.local")).expect("env"),
        format!(
            "DATABASE_URL=postgres://127.0.0.1:{db}/app\nAPI_URL=http://127.0.0.1:{db}/rest\n\
             PORT={web}\n"
        )
    );
}

/// The one conflict left: two resources claiming the same text. Refused at
/// load, naming both resources, both finds, and the way out.
#[test]
fn two_resources_claiming_the_same_text_are_refused_at_load() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, ENV);
    write_resource(&store, "db", DB);
    write_resource(
        &store,
        "web",
        r#"ownership = "branch"

[[render]]
path = ".env.local"
replace = [
  { find = "54322", with = "1" },
]
"#,
    );

    let Err(error) = BranchManager::open(store) else {
        panic!("a claim inside another resource's claim must be refused");
    };
    assert!(
        matches!(error, NewgitError::RenderPathConflict { .. }),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("`db`"), "{message}");
    assert!(message.contains("`web`"), "{message}");
    assert!(message.contains("54322"), "{message}");
    assert!(message.contains("{{exports."), "{message}");
}

/// An overlap that only the file's content reveals — one find's tail is the
/// other's head — is refused when it renders, with the same error.
#[test]
fn claims_that_overlap_in_the_content_are_refused_at_render() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, "HOST=localhost:3000\n");
    write_resource(
        &store,
        "db",
        r#"ownership = "branch"

[[render]]
path = ".env.local"
replace = [
  { find = "HOST=localhost", with = "HOST=db.local" },
]
"#,
    );
    write_resource(
        &store,
        "web",
        r#"ownership = "branch"

[[render]]
path = ".env.local"
replace = [
  { find = "localhost:3000", with = "localhost:3100" },
]
"#,
    );
    let m = BranchManager::open(store).expect("loads: the finds are disjoint as strings");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let web = spawned
        .resources
        .iter()
        .find(|outcome| outcome.name == "web")
        .expect("outcome");
    let error = web.render_error.as_deref().expect("render error");
    assert!(error.contains("both claim the same text"), "{error}");
    assert!(error.contains("`db`") && error.contains("`web`"), "{error}");
}

/// A tracker-owned shared file: capture takes every resource's values out
/// before recording, in one pass, and keeps the edit made beside them.
#[test]
fn capture_reverses_every_resources_values_in_a_shared_file() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, "README=1\n");
    std::fs::write(store.paths().project_root.join(".env.local"), ENV).expect("write env");
    write_resource(
        &store,
        "db",
        &DB.replace("ownership", "depends_on = [\"env\"]\nownership"),
    );
    write_resource(
        &store,
        "web",
        &WEB.replace("ownership", "depends_on = [\"env\"]\nownership"),
    );
    let m = BranchManager::open(store).expect("manager");
    m.create_tracker("env", "user", Storage::Local, false)
        .expect("create tracker");
    m.track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    m.seed_tracker_from_store("env").expect("seed");
    let m = BranchManager::open(MetadataStore::at(m.store().paths().project_root.clone()))
        .expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let (db, web) = ports(&m, "feature-a");
    let edited = format!("{}STRIPE_KEY=sk_test_123\n", rendered(db, web));
    std::fs::write(workspace.join(".env.local"), &edited).expect("edit");

    let report = m.capture_tracker("feature-a", "env").expect("capture");
    let paths = m.store().paths();
    let lane = newgit_core::tracker_repo::TrackerRepo::new(
        &paths.tracker_store,
        &paths.tracker_checkouts,
        "env",
    )
    .read_file(&report.rev, Utf8Path::new(".env.local"))
    .expect("read lane")
    .expect("lane content");
    assert_eq!(lane, format!("{ENV}STRIPE_KEY=sk_test_123\n"));

    // The workspace comes back whole, not with half the values reversed.
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("after"),
        edited
    );
}

/// Undo re-renders every claim, and drift names every resource rendering
/// the file.
#[test]
fn undo_re_renders_every_claim_and_drift_names_every_owner() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, ENV);
    write_resource(&store, "db", DB);
    write_resource(&store, "web", WEB);
    let m = BranchManager::open(store).expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let (db, web) = ports(&m, "feature-a");

    let clean = m
        .checkpoint("feature-a", Some("before"))
        .expect("checkpoint");
    assert!(
        !clean.warnings.iter().any(|w| w.contains(".env.local")),
        "{:?}",
        clean.warnings
    );

    std::fs::write(workspace.join(".env.local"), "wrecked\n").expect("wreck");
    let undone = m.undo("feature-a", &UndoOptions::default()).expect("undo");
    let warning = undone
        .warnings
        .iter()
        .find(|w| w.contains(".env.local"))
        .expect("drift warning");
    assert!(warning.contains("resources `db` and `web`"), "{warning}");
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("env"),
        rendered(db, web)
    );
}

/// Dropping the tracker that brings `web` takes `web`'s port out of the
/// shared file and leaves `db`'s value in it; using it again puts a port
/// back.
#[test]
fn unbinding_one_resource_takes_only_its_values_out() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, ENV);
    write_resource(&store, "db", DB);
    write_resource(&store, "web", WEB);
    std::fs::write(
        store.paths().trackers.join("web-env.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.web\"]\nresources = [\"web\"]\n",
    )
    .expect("tracker");
    let m = BranchManager::open(store).expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let (db, _) = ports(&m, "feature-a");

    m.drop_tracker("feature-a", "web-env", Uncommitted::Refuse)
        .expect("drop");
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("env"),
        format!("DATABASE_URL=postgres://127.0.0.1:{db}/app\nPORT=3000\nDEBUG=1\n")
    );

    m.use_tracker("feature-a", "web-env").expect("use");
    let (db, web) = ports(&m, "feature-a");
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("env"),
        rendered(db, web)
    );
}

/// When the committed file moves under one resource's `find`, that
/// resource's render fails on its own; the other resource on the file still
/// renders its value instead of failing with it.
#[test]
fn a_stale_claim_does_not_block_another_resources_render() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, ENV);
    write_resource(&store, "db", DB);
    write_resource(&store, "web", WEB);
    let m = BranchManager::open(store).expect("manager");

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let (db, _) = ports(&m, "feature-a");

    // Commit a `.env.local` with no `PORT=3000` for `web` to find.
    let moved = "DATABASE_URL=postgres://127.0.0.1:54322/app\nDEBUG=1\n";
    git(
        &workspace,
        &["update-index", "--no-skip-worktree", ".env.local"],
    );
    std::fs::write(workspace.join(".env.local"), moved).expect("write");
    git(&workspace, &["commit", "-q", "-am", "no PORT"]);
    m.checkpoint("feature-a", Some("moved"))
        .expect("checkpoint");

    let undone = m.undo("feature-a", &UndoOptions::default()).expect("undo");
    assert!(
        undone
            .warnings
            .iter()
            .any(|w| w.contains("`web`") && w.contains("PORT=3000")),
        "{:?}",
        undone.warnings
    );
    assert!(
        !undone
            .warnings
            .iter()
            .any(|w| w.contains("re-render for resource `db` failed")),
        "{:?}",
        undone.warnings
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("env"),
        format!("DATABASE_URL=postgres://127.0.0.1:{db}/app\nDEBUG=1\n")
    );
}
