//! `[checkpoint]`, `[restore]` and `[cleanup]` are not actions, and the
//! reference used to scope the command environment to `newgit run` and
//! actions everywhere it mentioned it (#77). They do get the environment —
//! all of it — and a `[restore]` that reads the allocated port to pick which
//! database it resets is the case where guessing wrong is destructive and
//! silent. The claim is now written down, so it is pinned here too.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::ArchivedCheckpoints;
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

/// Each hook writes the three kinds of variable it is documented to receive:
/// another resource's `[exports]`, a port's `env`, and `NEWGIT_*`. A hook
/// that got an empty environment would write empty fields rather than fail,
/// so the assertions check the values, not the exit status.
#[test]
fn checkpoint_restore_and_cleanup_hooks_all_get_the_command_environment() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    // The export lives on a *different* resource than the hooks that read it:
    // the environment is per instance, not per resource.
    write_resource(
        &store,
        "infra",
        r#"ownership = "branch"

[exports]
COMPOSE_PROJECT = "csr-{{branch.slug}}"
"#,
    );

    // `db` owns the port and carries all three hooks. Each one records what it
    // actually saw, tagged by hook name.
    write_resource(
        &store,
        "db",
        &format!(
            r#"ownership = "branch"

[ports.pg]
start = 55432
env = "PG_PORT"

[checkpoint]
mode = "command"
command = "echo checkpoint $COMPOSE_PROJECT $PG_PORT $NEWGIT_BRANCH $NEWGIT_WORKSPACE >> {witness}/env.txt && echo handle-1"

[restore]
mode = "command"
command = "echo restore $COMPOSE_PROJECT $PG_PORT $NEWGIT_BRANCH $NEWGIT_WORKSPACE >> {witness}/env.txt"

[cleanup]
command = "echo cleanup $COMPOSE_PROJECT $PG_PORT $NEWGIT_BRANCH $NEWGIT_WORKSPACE >> {witness}/env.txt"
"#
        ),
    );

    let manager = manager_at(&store);
    assert!(manager.graph_problems().is_empty());
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();
    let port = spawned.branch.resources["db"].resolved_ports["pg"];

    manager
        .checkpoint("feature-a", Some("v1"))
        .expect("checkpoint");
    manager
        .undo("feature-a", &UndoOptions::default())
        .expect("undo");
    manager
        .remove("feature-a", &temp, ArchivedCheckpoints::Keep)
        .expect("remove");

    let expected_tail = format!("csr-feature-a {port} feature-a {workspace}");
    let seen = std::fs::read_to_string(witness.join("env.txt")).expect("read env.txt");
    let lines: Vec<&str> = seen.lines().collect();

    for hook in ["checkpoint", "restore", "cleanup"] {
        let line = lines
            .iter()
            .find(|line| line.starts_with(&format!("{hook} ")))
            .unwrap_or_else(|| panic!("`{hook}` hook never ran; saw: {lines:?}"));
        assert_eq!(
            *line,
            format!("{hook} {expected_tail}"),
            "`{hook}` did not get the full command environment"
        );
    }
}
