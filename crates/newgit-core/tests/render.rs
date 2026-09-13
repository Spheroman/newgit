//! Render: per-instance values substituted into committed config files.
//!
//! The unit tests in `render.rs` cover the substitution itself. These cover
//! the part that only exists end to end — that the instance's values reach
//! the workspace and reach *nothing else*: not `git status`, not a
//! checkpoint, not an export, not a shared tracker lane.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;
use newgit_core::{NewgitError, SourceSubstrate};

const SUPABASE_CONFIG: &str =
    "project_id = \"faretable\"\n\n[api]\nport = 54321\n\n[db]\nport = 54322\n";

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

/// A store repo whose committed `supabase/config.toml` holds the project's
/// working defaults — the file a clone without newgit would start on.
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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    store.write_resource_file(name, contents).expect("resource");
}

fn manager(store: MetadataStore) -> BranchManager {
    BranchManager::open(store).expect("manager")
}

/// The shape from issue #10: ports and a project id, straight into the
/// committed Supabase config.
const SUPABASE_RESOURCE: &str = r#"kind = "command"
ownership = "branch"

[ports]
api = { start = 54400 }
db = { start = 54500 }

[[render]]
path = "supabase/config.toml"
replace = [
  { find = 'project_id = "faretable"', with = 'project_id = "faretable-{{branch.slug}}"' },
  { find = "[api]\nport = 54321", with = "[api]\nport = {{ports.api}}" },
  { find = "[db]\nport = 54322", with = "[db]\nport = {{ports.db}}" },
]
"#;

#[test]
fn render_substitutes_this_instances_values_into_the_committed_file() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let rendered =
        std::fs::read_to_string(workspace.join("supabase/config.toml")).expect("rendered");

    let api = spawned.branch.resources["supabase"].resolved_ports["api"];
    let db = spawned.branch.resources["supabase"].resolved_ports["db"];
    // Byte-exact, including the trailing newline: a render rewrites the
    // values it names and nothing else about the file.
    assert_eq!(
        rendered,
        format!(
            "project_id = \"faretable-feature-a\"\n\n[api]\nport = {api}\n\n[db]\nport = {db}\n"
        )
    );

    // The store's copy is untouched: the base repo still starts on its own
    // defaults, which is the whole point of rendering into committed content.
    let base = std::fs::read_to_string(m.store().paths().project_root.join("supabase/config.toml"))
        .expect("base");
    assert_eq!(base, SUPABASE_CONFIG);

    let outcome = spawned
        .resources
        .iter()
        .find(|resource| resource.name == "supabase")
        .expect("outcome");
    assert_eq!(outcome.rendered.len(), 1);
    assert_eq!(outcome.rendered[0].replacements, 3);
}

/// The lossiness is reported when it is real, not as a caveat at bind. A
/// clean instance has nothing to say about it.
#[test]
fn a_checkpoint_is_silent_about_a_rendered_file_nobody_touched() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    m.spawn("feature-a", None).expect("spawn");
    let outcome = m
        .checkpoint("feature-a", Some("clean"))
        .expect("checkpoint");
    assert!(
        !outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("supabase/config.toml")),
        "warned with nothing to warn about: {:?}",
        outcome.warnings
    );
}

/// ...and speaks at the moment the edit is about to be discarded, naming the
/// file rather than restating the general rule.
#[test]
fn a_checkpoint_names_hand_edits_a_render_will_discard() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let config = spawned.branch.workspace_path.join("supabase/config.toml");

    // The edit the warning exists for: a real change to a rendered file,
    // which the checkpoint cannot carry.
    let edited = std::fs::read_to_string(&config).expect("read") + "\n[auth]\nenabled = true\n";
    std::fs::write(&config, edited).expect("edit");

    let outcome = m.checkpoint("feature-a", Some("work")).expect("checkpoint");
    let warning = outcome
        .warnings
        .iter()
        .find(|warning| warning.contains("supabase/config.toml"))
        .expect("drift warning");
    assert!(warning.contains("render will discard"), "{warning}");
    assert!(warning.contains("line(s) differ"), "{warning}");
}

/// Drift is detected against the recomputed render, not against committed
/// content — the instance's own rendered values must not read as an edit.
#[test]
fn an_undo_re_render_reports_the_edit_it_is_about_to_overwrite() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let config = spawned.branch.workspace_path.join("supabase/config.toml");
    m.checkpoint("feature-a", Some("before"))
        .expect("checkpoint");

    let edited = std::fs::read_to_string(&config).expect("read") + "\n[auth]\nenabled = true\n";
    std::fs::write(&config, edited).expect("edit");

    let undone = m.undo("feature-a", None).expect("undo");
    assert!(
        undone
            .warnings
            .iter()
            .any(|warning| warning.contains("supabase/config.toml")
                && warning.contains("render will discard")),
        "undo overwrote a hand edit silently: {:?}",
        undone.warnings
    );
}

#[test]
fn two_instances_render_different_ports_into_the_same_committed_file() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    let a = m.spawn("feature-a", None).expect("spawn a");
    let b = m.spawn("feature-b", None).expect("spawn b");

    let a_api = a.branch.resources["supabase"].resolved_ports["api"];
    let b_api = b.branch.resources["supabase"].resolved_ports["api"];
    assert_ne!(a_api, b_api);

    let a_config =
        std::fs::read_to_string(a.branch.workspace_path.join("supabase/config.toml")).expect("a");
    let b_config =
        std::fs::read_to_string(b.branch.workspace_path.join("supabase/config.toml")).expect("b");
    assert!(a_config.contains(&format!("port = {a_api}")));
    assert!(b_config.contains(&format!("port = {b_api}")));
    assert!(a_config.contains("faretable-feature-a"));
    assert!(b_config.contains("faretable-feature-b"));
}

/// skip-worktree, doing its job: an agent running `git add -A && git commit`
/// in the workspace cannot commit this instance's ports.
#[test]
fn a_rendered_source_file_is_invisible_to_git_status_and_to_git_add() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();

    let status = git(&workspace, &["status", "--porcelain"]);
    assert!(
        !status.contains("supabase/config.toml"),
        "rendered file showed in git status: {status}"
    );

    git(&workspace, &["add", "-A"]);
    let staged = git(&workspace, &["diff", "--cached", "--name-only"]);
    assert!(
        !staged.contains("supabase/config.toml"),
        "rendered file was staged by `git add -A`: {staged}"
    );
}

/// The leak the design review caught: `--skip-worktree` does not remove a
/// path from `git ls-files`, and export copies tracked files from disk.
#[test]
fn export_ships_committed_content_not_this_instances_ports() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let api = spawned.branch.resources["supabase"].resolved_ports["api"];
    let destination = temp.join("export");
    m.export("feature-a", &destination, &Default::default())
        .expect("export");

    let exported =
        std::fs::read_to_string(destination.join("supabase/config.toml")).expect("exported");
    assert_eq!(exported, SUPABASE_CONFIG);
    assert!(!exported.contains(&format!("port = {api}")));
}

/// The other half of the same leak: a checkpoint's dirty commit is built in a
/// throwaway index seeded from HEAD, which does not carry skip-worktree bits.
#[test]
fn a_checkpoint_does_not_capture_this_instances_rendered_ports() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let api = spawned.branch.resources["supabase"].resolved_ports["api"];

    // Real uncommitted work alongside the rendered file, so the checkpoint
    // has something to capture and the rendered file is not simply the only
    // difference.
    std::fs::write(workspace.join("README.md"), "hello agent\n").expect("write");

    let outcome = m.checkpoint("feature-a", Some("work")).expect("checkpoint");
    let tip = outcome
        .record
        .source
        .dirty_rev
        .clone()
        .expect("dirty commit");

    let captured = git(
        &workspace,
        &["show", &format!("{tip}:supabase/config.toml")],
    );
    assert_eq!(captured, SUPABASE_CONFIG);
    assert!(!captured.contains(&format!("port = {api}")));
    // The agent's real work still made it in.
    let readme = git(&workspace, &["show", &format!("{tip}:README.md")]);
    assert_eq!(readme, "hello agent\n");
}

/// The drift detector. Upstream bumps its default and bind fails naming the
/// string, instead of silently leaving the file unrendered.
#[test]
fn a_find_that_stopped_matching_fails_the_resource_loudly() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "supabase",
        r#"kind = "command"
ownership = "branch"

[ports]
api = { start = 54400 }

[[render]]
path = "supabase/config.toml"
replace = [
  { find = "port = 59999", with = "port = {{ports.api}}" },
]
"#,
    );
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let outcome = spawned
        .resources
        .iter()
        .find(|resource| resource.name == "supabase")
        .expect("outcome");

    let error = outcome.render_error.as_deref().expect("render error");
    assert!(error.contains("port = 59999"), "{error}");
    assert!(error.contains("supabase/config.toml"), "{error}");
    assert_eq!(
        spawned.branch.resources["supabase"].status,
        newgit_core::branch::ResourceStatus::Failed
    );
}

#[test]
fn rendering_into_a_path_with_no_committed_content_is_refused() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "app",
        r#"kind = "command"
ownership = "branch"

[ports]
api = { start = 54400 }

[[render]]
path = "not/committed.toml"
replace = [
  { find = "port = 1", with = "port = {{ports.api}}" },
]
"#,
    );
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let outcome = spawned
        .resources
        .iter()
        .find(|resource| resource.name == "app")
        .expect("outcome");
    let error = outcome.render_error.as_deref().expect("render error");
    assert!(
        error.contains("neither tracked by Git nor owned by a tracker"),
        "{error}"
    );
}

#[test]
fn two_resources_rendering_one_path_is_refused_at_load() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let definition = r#"kind = "command"
ownership = "branch"

[[render]]
path = "supabase/config.toml"
replace = [
  { find = "port = 54321", with = "port = 1" },
]
"#;
    write_resource(&store, "one", definition);
    write_resource(&store, "two", definition);

    assert!(matches!(
        BranchManager::open(store),
        Err(NewgitError::RenderPathConflict { .. })
    ));
}

// -- tracker-owned renders: the case a template file could not serve --------

const ENV_RESOURCE: &str = r#"kind = "command"
ownership = "branch"
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
    std::fs::write(
        store.paths().project_root.join(".env.local"),
        "SUPABASE_URL=http://127.0.0.1:54321\n",
    )
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

#[test]
fn a_tracker_owned_file_renders_from_the_lane() {
    let (_guard, temp) = tempdir();
    let store = setup_env_tracker(&temp);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let api = spawned.branch.resources["supabase"].resolved_ports["api"];
    let env =
        std::fs::read_to_string(spawned.branch.workspace_path.join(".env.local")).expect("env");
    assert_eq!(env, format!("SUPABASE_URL=http://127.0.0.1:{api}\n"));
}

/// The payoff of invertibility: capture keeps the key you added and drops the
/// port you were allocated, so the lane stays shareable.
#[test]
fn capture_reverses_the_render_and_keeps_edits_made_beside_it() {
    let (_guard, temp) = tempdir();
    let store = setup_env_tracker(&temp);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let api = spawned.branch.resources["supabase"].resolved_ports["api"];

    // An edit a developer would actually make, alongside the rendered value.
    let edited = format!("SUPABASE_URL=http://127.0.0.1:{api}\nSTRIPE_KEY=sk_test_123\n");
    std::fs::write(workspace.join(".env.local"), &edited).expect("edit");

    let report = m
        .capture_tracker("feature-a", "runtime-env")
        .expect("capture");

    let lane_content = std::fs::read_to_string(
        m.store()
            .paths()
            .snapshots
            .join("runtime-env")
            .join(&report.rev)
            .join(".env.local"),
    )
    .expect("lane content");
    assert_eq!(
        lane_content,
        "SUPABASE_URL=http://127.0.0.1:54321\nSTRIPE_KEY=sk_test_123\n"
    );

    // The workspace is untouched by the capture: the instance is still
    // running against its own port.
    let after = std::fs::read_to_string(workspace.join(".env.local")).expect("after");
    assert_eq!(after, edited);
}

/// Undo moves content underneath the rendered files; the instance's values go
/// back on top, for free, from the binding record.
#[test]
fn undo_re_renders_this_instances_values() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "supabase", SUPABASE_RESOURCE);
    let m = manager(store);

    let spawned = m.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let api = spawned.branch.resources["supabase"].resolved_ports["api"];

    m.checkpoint("feature-a", Some("before"))
        .expect("checkpoint");
    std::fs::write(workspace.join("README.md"), "agent wrecked it\n").expect("write");
    let undone = m.undo("feature-a", None).expect("undo");
    assert!(undone.is_complete(), "{:?}", undone.warnings);

    let rendered =
        std::fs::read_to_string(workspace.join("supabase/config.toml")).expect("rendered");
    assert!(
        rendered.contains(&format!("port = {api}")),
        "undo left the committed default in place: {rendered}"
    );
}
