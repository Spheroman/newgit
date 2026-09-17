//! Two contracts the reference stated ambiguously, now stated precisely and
//! pinned here.
//!
//! #78: a resource's ports and `[exports]` are bound before its own `prepare`
//! runs. Both were documented as happening "at `spawn`" with no order between
//! them, so a `prepare` consuming its own export had to guess — and guessing
//! wrong brings up the shared stack under the committed default name.
//!
//! #81: the reference said action names are inert and, three paragraphs
//! earlier, documented a `signal` default keyed on the literal name `stop`.
//! `stop` is read; the part that was genuinely undetermined is what a `stop`
//! action *with a command* does.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::manager::BranchManager;
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

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(newgit_core::config::WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

fn manager_at(store: &MetadataStore) -> BranchManager {
    BranchManager::open(MetadataStore::at(store.paths().project_root.clone())).expect("manager")
}

fn app_definition(manager: &BranchManager) -> &newgit_core::resource::ResourceDefinition {
    manager
        .resource_definitions()
        .iter()
        .find(|definition| definition.name == "app")
        .expect("definition")
}

/// #78. The `prepare` writes what it saw; an export bound *after* it would
/// leave the field empty rather than fail, so the assertion reads the value.
#[test]
fn a_resources_own_ports_and_exports_are_bound_before_its_own_prepare() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    write_resource(
        &store,
        "infra",
        &format!(
            r#"ownership = "branch"

[ports.app]
start = 55700
env = "APP_PORT"

[exports]
COMPOSE_PROJECT = "csr-{{{{branch.slug}}}}"

[actions.prepare]
command = "echo $COMPOSE_PROJECT $APP_PORT > {witness}/prepare.txt"
"#
        ),
    );

    let manager = manager_at(&store);
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let port = spawned.branch.resources["infra"].resolved_ports["app"];

    let seen = std::fs::read_to_string(witness.join("prepare.txt")).expect("prepare ran");
    assert_eq!(seen.trim(), format!("csr-feature-a {port}"));
}

/// #78, the dependency half: a dependency's exports are bound an iteration
/// earlier, so a dependent's `prepare` sees them too.
#[test]
fn a_dependencys_exports_are_bound_before_a_dependents_prepare() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    write_resource(
        &store,
        "infra",
        r#"ownership = "branch"

[exports]
COMPOSE_PROJECT = "csr-{{branch.slug}}"
"#,
    );
    write_resource(
        &store,
        "db",
        &format!(
            r#"ownership = "branch"
depends_on = ["infra"]

[actions.prepare]
command = "echo $COMPOSE_PROJECT > {witness}/db-prepare.txt"
"#
        ),
    );

    let manager = manager_at(&store);
    assert!(manager.graph_problems().is_empty());
    manager.spawn("feature-a", None).expect("spawn");

    let seen = std::fs::read_to_string(witness.join("db-prepare.txt")).expect("prepare ran");
    assert_eq!(seen.trim(), "csr-feature-a");
}

/// #81. A signal-only `stop` is the case the `signal` default is written for.
#[test]
fn a_signal_only_stop_action_supplies_the_signal_newgit_sends() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);

    write_resource(
        &store,
        "app",
        r#"ownership = "branch"

[actions.start]
command = "sleep 60"
long_running = true

[actions.stop]
signal = "int"
"#,
    );

    let manager = manager_at(&store);
    let definition = app_definition(&manager);
    assert_eq!(definition.stop_signal(), "int");
}

/// #81, the part the issue could not determine from outside. An action named
/// `stop` that has a `command` is accepted and is an ordinary action — but
/// newgit's own stops always signal, so the command is never what they run,
/// and with no `signal` set the resource gets a bare `term`.
#[test]
fn a_stop_action_with_a_command_is_ordinary_and_leaves_the_signal_at_term() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    write_resource(
        &store,
        "app",
        &format!(
            r#"ownership = "branch"

[actions.start]
command = "sleep 60"
long_running = true

[actions.stop]
command = "echo ran > {witness}/stop.txt"
"#
        ),
    );

    let manager = manager_at(&store);

    // Accepted at load: a command satisfies the "command or signal" rule, and
    // the name `stop` does not make it signal-only.
    let definition = app_definition(&manager);
    assert!(definition.actions["stop"].command.is_some());

    // No `signal` was set, so newgit's own stops fall back to TERM — the
    // command is not consulted.
    assert_eq!(definition.stop_signal(), "term");

    // And invoking it by name runs the command like any other action.
    manager.spawn("feature-a", None).expect("spawn");
    manager.run_action("feature-a", "app.stop").expect("action");
    assert_eq!(
        std::fs::read_to_string(witness.join("stop.txt"))
            .expect("stop command ran")
            .trim(),
        "ran"
    );
}
