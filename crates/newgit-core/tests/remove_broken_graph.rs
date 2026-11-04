//! Teardown must stay reachable from a broken resource graph, as `remove`'s
//! doc comment promises: "Deliberately not gated on
//! [`Self::require_resolvable_graph`]: teardown must stay reachable from a
//! broken graph". That is `remove`, and the `cleanup` sweep finishing an
//! instance whose workspace is gone.
//!
//! Both run each bound resource's `[cleanup]` hook, and the hook's
//! environment used to come from `assemble_env` → `command_env`, which
//! begins with `require_resolvable_graph`. So one bad `depends_on` anywhere
//! in the project made every instance with a cleanup hook impossible to
//! remove, even one that binds only valid resources.
//!
//! What teardown still refuses is narrower: a name the graph declares twice
//! that reaches this instance's environment with two values, where a hook
//! could act on the wrong one.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, SourceBranchPolicy};
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

/// `db` picks what it tears down by `PG_PORT`, so its hook has to get its
/// own port and not another resource's.
fn write_db(store: &MetadataStore, witness: &Utf8Path) {
    write_resource(
        store,
        "db",
        &format!(
            r#"ownership = "branch"

[ports.pg]
start = 55432
env = "PG_PORT"

[cleanup]
command = "echo dropping $PG_PORT >> {witness}/cleanup.txt"
"#
        ),
    );
}

/// A resource with one port, published to every command as `env`.
fn port_resource(start: u16, env: &str) -> String {
    format!(
        r#"ownership = "branch"

[ports.main]
start = {start}
env = "{env}"
"#
    )
}

#[test]
fn remove_tears_down_an_instance_while_the_resource_graph_is_broken() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    // A valid, branch-owned resource with a teardown hook: exactly what
    // `remove` exists to run.
    write_resource(
        &store,
        "db",
        &format!(
            r#"ownership = "branch"

[actions.prepare]
command = "true"

[cleanup]
command = "echo db >> {witness}/cleanup.txt"
"#
        ),
    );
    manager_at(&store).spawn("feature-a", None).expect("spawn");

    // Later, the project gains a resource whose dependency does not exist.
    // `open` reports that rather than raising it.
    write_resource(
        &store,
        "broken",
        r#"ownership = "branch"
depends_on = ["missing"]

[actions.prepare]
command = "true"
"#,
    );
    let manager = manager_at(&store);
    assert!(
        !manager.graph_problems().is_empty(),
        "precondition: the graph is broken"
    );

    let outcome = match manager.remove(
        "feature-a",
        &temp,
        ArchivedCheckpoints::Keep,
        SourceBranchPolicy::Auto,
    ) {
        Ok(outcome) => outcome,
        Err(error) => panic!(
            "remove refused while the graph is broken, although `feature-a` binds only \
             `db`, which is valid: {error}"
        ),
    };

    assert!(outcome.archived_record.is_file(), "the record is archived");
    let branches = store.load_branches().expect("load branches");
    assert!(
        !branches.iter().any(|branch| branch.name == "feature-a"),
        "the live record is gone"
    );
    assert_eq!(
        std::fs::read_to_string(witness.join("cleanup.txt")).expect("cleanup ran"),
        "db\n",
        "and the bound resource's cleanup hook ran"
    );
}

/// `cleanup` finishes the teardown of an instance whose workspace is gone,
/// with the same hooks, so it has to get past a broken graph too. The hook
/// reads another resource's export: an edge that does not resolve is
/// skipped, not fatal, so every resource is still in the order the
/// environment is layered in.
#[test]
fn cleanup_finishes_a_workspaceless_instance_while_the_resource_graph_is_broken() {
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

[cleanup]
command = "echo db $COMPOSE_PROJECT $NEWGIT_BRANCH >> {witness}/cleanup.txt"
"#
        ),
    );
    let spawned = manager_at(&store).spawn("feature-a", None).expect("spawn");

    // Someone deleted the workspace directly, and the graph broke since.
    std::fs::remove_dir_all(&spawned.branch.workspace_path).expect("rm -rf workspace");
    write_resource(
        &store,
        "broken",
        r#"ownership = "branch"
depends_on = ["missing"]

[actions.prepare]
command = "true"
"#,
    );
    let manager = manager_at(&store);
    assert!(
        !manager.graph_problems().is_empty(),
        "precondition: the graph is broken"
    );

    let outcome = match manager.cleanup(false, ArchivedCheckpoints::Keep) {
        Ok(outcome) => outcome,
        Err(error) => panic!("cleanup refused while the graph is broken: {error}"),
    };

    assert_eq!(outcome.finalized.len(), 1, "the sweep finds the instance");
    let finalized = &outcome.finalized[0];
    assert_eq!(finalized.name, "feature-a");
    assert!(
        finalized
            .archived_record
            .as_ref()
            .is_some_and(|record| record.is_file()),
        "the record is archived"
    );
    let branches = store.load_branches().expect("load branches");
    assert!(
        !branches.iter().any(|branch| branch.name == "feature-a"),
        "the live record is gone"
    );
    assert_eq!(
        std::fs::read_to_string(witness.join("cleanup.txt")).expect("cleanup ran"),
        format!("db csr-{} feature-a\n", spawned.branch.slug),
        "and the hook got the command environment"
    );
}

/// The one thing teardown still refuses: a name two declarations claim, when
/// both reach this instance with different values. A port's `env` is read
/// from the definition every time, so renaming `redis`'s onto `PG_PORT`
/// after the spawn hands each hook in the instance both ports, and `db`'s
/// would have dropped whatever the one layered last points at. Nothing may
/// be torn down, and renaming it back is the way out.
#[test]
fn remove_refuses_while_one_name_reaches_the_instance_with_two_values() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    write_db(&store, &witness);
    write_resource(&store, "redis", &port_resource(56379, "REDIS_PORT"));
    let spawned = manager_at(&store).spawn("feature-a", None).expect("spawn");
    let pg_port = spawned.branch.resources["db"].resolved_ports["pg"];

    write_resource(&store, "redis", &port_resource(56379, "PG_PORT"));
    let error = manager_at(&store)
        .remove(
            "feature-a",
            &temp,
            ArchivedCheckpoints::Keep,
            SourceBranchPolicy::Auto,
        )
        .expect_err("remove refuses");
    let message = error.to_string();
    assert!(
        message.contains("`PG_PORT`")
            && message.contains("`db` [ports.pg] env")
            && message.contains("`redis` [ports.main] env"),
        "the refusal names the variable and both claimants: {message}"
    );
    assert!(!witness.join("cleanup.txt").exists(), "no hook ran");
    assert!(
        spawned.branch.workspace_path.is_dir(),
        "the workspace is still there"
    );
    assert!(
        store.find_branch("feature-a").is_ok(),
        "and so is the record"
    );

    // Renamed back, the instance has one `PG_PORT` again: `db`'s own.
    write_resource(&store, "redis", &port_resource(56379, "REDIS_PORT"));
    manager_at(&store)
        .remove(
            "feature-a",
            &temp,
            ArchivedCheckpoints::Keep,
            SourceBranchPolicy::Auto,
        )
        .expect("remove");
    assert_eq!(
        std::fs::read_to_string(witness.join("cleanup.txt")).expect("cleanup ran"),
        format!("dropping {pg_port}\n")
    );
}

/// A collision blocks teardown only where it reaches the instance. `api` is
/// added after the spawn, so `feature-a` never bound it, and its `PG_PORT`
/// leaves this instance's environment with one value: `db`'s own.
#[test]
fn a_collision_the_instance_never_bound_does_not_block_remove() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("witness");
    std::fs::create_dir_all(&witness).expect("mkdir");

    write_db(&store, &witness);
    let spawned = manager_at(&store).spawn("feature-a", None).expect("spawn");
    let pg_port = spawned.branch.resources["db"].resolved_ports["pg"];

    write_resource(&store, "api", &port_resource(56380, "PG_PORT"));
    let manager = manager_at(&store);
    assert!(
        !manager.graph_problems().is_empty(),
        "precondition: the graph is broken"
    );

    manager
        .remove(
            "feature-a",
            &temp,
            ArchivedCheckpoints::Keep,
            SourceBranchPolicy::Auto,
        )
        .expect("remove");
    assert_eq!(
        std::fs::read_to_string(witness.join("cleanup.txt")).expect("cleanup ran"),
        format!("dropping {pg_port}\n")
    );
}
