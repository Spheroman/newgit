//! Roles, priority, `requires`, and trackers that bring resources
//! (newgit-trackers.md, step 4): what an instance binds by default, what it
//! binds when asked, and what follows from that.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BindingChoice, BranchManager, Uncommitted};
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

fn define(store: &MetadataStore, name: &str, toml: &str) {
    std::fs::write(store.paths().trackers.join(format!("{name}.toml")), toml).expect("write");
}

/// A `db` role with three trackers, an `api` that requires the role, a
/// role-less tracker only bound when asked for, and a resource `dev-db` brings.
fn project(temp: &Utf8Path) -> BranchManager {
    let store = setup(temp);
    define(
        &store,
        "dev-db",
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.db\"]\nrole = \"db\"\npriority = 1\nresources = [\"pg\"]\n",
    );
    define(
        &store,
        "staging-db",
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.db\"]\nrole = \"db\"\n",
    );
    define(
        &store,
        "prod-db",
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.db\"]\nrole = \"db\"\npriority = -1\n",
    );
    define(
        &store,
        "api",
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.api\"]\nrequires = [\"db\"]\n",
    );
    define(
        &store,
        "extras",
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.extras\"]\npriority = -1\n",
    );
    std::fs::write(
        store.paths().resources.join("pg.toml"),
        "ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"true\"\n",
    )
    .expect("resource");
    BranchManager::open(store).expect("manager")
}

fn dropped(manager: &BranchManager, instance: &str) -> Vec<String> {
    manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .dropped
        .into_iter()
        .collect()
}

fn resources(manager: &BranchManager, instance: &str) -> Vec<String> {
    manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .resources
        .into_keys()
        .collect()
}

#[test]
fn a_spawn_binds_the_highest_priority_per_role_and_what_it_brings() {
    let (_guard, temp) = tempdir();
    let manager = project(&temp);
    manager.spawn("feature-a", None).expect("spawn");
    assert_eq!(
        dropped(&manager, "feature-a"),
        ["extras", "prod-db", "staging-db"],
        "one per role; negative priority only when asked"
    );
    assert_eq!(resources(&manager, "feature-a"), ["pg"]);
}

#[test]
fn asking_for_a_tracker_fills_its_role_instead() {
    let (_guard, temp) = tempdir();
    let manager = project(&temp);
    manager
        .spawn_binding(
            "feature-a",
            None,
            &BindingChoice {
                uses: vec!["staging-db".to_owned(), "extras".to_owned()],
                drops: Vec::new(),
                ..Default::default()
            },
        )
        .expect("spawn");
    assert_eq!(dropped(&manager, "feature-a"), ["dev-db", "prod-db"]);
    assert!(
        resources(&manager, "feature-a").is_empty(),
        "staging-db brings nothing"
    );
}

#[test]
fn a_required_role_cannot_be_left_empty() {
    let (_guard, temp) = tempdir();
    let manager = project(&temp);
    let refused = manager
        .spawn_binding(
            "feature-a",
            None,
            &BindingChoice {
                uses: Vec::new(),
                drops: vec!["dev-db".to_owned(), "staging-db".to_owned()],
                ..Default::default()
            },
        )
        .expect_err("api requires a db");
    assert!(refused.to_string().contains("requires a `db`"), "{refused}");
}

#[test]
fn using_another_tracker_in_a_role_swaps_it_and_its_resources() {
    let (_guard, temp) = tempdir();
    let manager = project(&temp);
    manager.spawn("feature-a", None).expect("spawn");
    let swap = manager.use_tracker("feature-a", "staging-db").expect("use");
    assert_eq!(swap.replaced, ["dev-db"]);
    assert_eq!(
        dropped(&manager, "feature-a"),
        ["dev-db", "extras", "prod-db"]
    );
    assert!(
        resources(&manager, "feature-a").is_empty(),
        "pg went with dev-db"
    );

    let back = manager
        .use_tracker("feature-a", "dev-db")
        .expect("use again");
    assert_eq!(back.started, ["pg"]);
}

#[test]
fn a_tracker_something_requires_cannot_be_dropped() {
    let (_guard, temp) = tempdir();
    let manager = project(&temp);
    manager.spawn("feature-a", None).expect("spawn");
    let refused = manager
        .drop_tracker("feature-a", "dev-db", Uncommitted::Refuse)
        .expect_err("api requires the db role");
    assert!(refused.to_string().contains("`api` requires"), "{refused}");
}
