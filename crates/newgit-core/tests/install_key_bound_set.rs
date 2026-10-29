//! The install store keys a built tree on the trackers its instance binds,
//! as docs/design/trackers.md promises under "Installs across trackers": a shared
//! lockfile lists every member, bound or not, its install builds only the
//! bound ones, "and the bound set is part of the install's identity, so
//! copy-on-write installs are keyed correctly".
//!
//! The key (`installs::identity_key`, from `BranchManager::install_key`)
//! used to mix only the identity paths' content, `key_command`'s output and
//! the definition's rev. A shared lockfile does not change with the binding,
//! so an instance that dropped a member was filled with the tree built for
//! one that bound it.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::config::WorkspaceSection;
use newgit_core::installs::InstallReport;
use newgit_core::manager::{BindingChoice, BranchManager, ResourceBindOutcome};
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

/// Builds the bound members, named by `{{bound.paths}}`.
const BOUND_INSTALL: &str =
    "mkdir -p node_modules && echo '{{bound.paths}}' > node_modules/members.txt";

/// Builds the members it finds on disk, and never mentions `{{bound…}}`.
const DISCOVERING_INSTALL: &str = "mkdir -p node_modules && ls apps > node_modules/members.txt";

/// A workspace-style monorepo: one root lockfile listing two members, each
/// member carved into its own code tracker, and a shared install keyed on
/// the lockfile that runs `prepare`.
fn setup(temp: &Utf8Path, prepare: &str) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("apps/a")).expect("mkdir");
    std::fs::create_dir_all(repo.join("apps/b")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("lock.txt"), "a@1\nb@1\n").expect("write");
    std::fs::write(repo.join("apps/a/index.js"), "a\n").expect("write");
    std::fs::write(repo.join("apps/b/index.js"), "b\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);

    for (name, path) in [("a", "apps/a"), ("b", "apps/b")] {
        BranchManager::open(store.clone())
            .expect("manager")
            .create_code_tracker(name, "project-devs", &[Utf8PathBuf::from(path)])
            .expect("carve");
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    let manager = BranchManager::open(store).expect("manager");
    // The project root's own commits reach `main` like anyone's.
    manager
        .merge("main", "main")
        .expect("merge the project root");

    std::fs::write(
        manager.store().paths().resources.join("deps.toml"),
        format!(
            r#"ownership = "workspace"

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]

[actions.prepare]
command = "{prepare}"
"#
        ),
    )
    .expect("resource");
    BranchManager::open(manager.store().clone()).expect("reopen")
}

fn drop_b() -> BindingChoice {
    BindingChoice {
        drops: vec!["b".to_owned()],
        ..Default::default()
    }
}

fn members(workspace: &Utf8Path) -> Vec<String> {
    let mut members: Vec<String> =
        std::fs::read_to_string(workspace.join("node_modules/members.txt"))
            .expect("the install's tree")
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect();
    members.sort();
    members
}

fn deps(resources: &[ResourceBindOutcome]) -> &ResourceBindOutcome {
    resources
        .iter()
        .find(|resource| resource.name == "deps")
        .expect("deps reported")
}

#[test]
fn an_instance_binding_fewer_trackers_is_not_filled_with_a_tree_built_for_more() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, BOUND_INSTALL);

    let both = manager.spawn("both", None).expect("spawn both");
    assert_eq!(
        members(&both.branch.workspace_path),
        ["apps/a", "apps/b"],
        "precondition: both bound"
    );
    let first = deps(&both.resources).install.clone();

    let only_a = manager
        .spawn_binding("only-a", None, &drop_b())
        .expect("spawn only-a");
    assert!(
        !only_a.branch.workspace_path.join("apps/b").exists(),
        "precondition: `b` is dropped"
    );
    let second = deps(&only_a.resources).install.clone();

    assert_eq!(
        members(&only_a.branch.workspace_path),
        ["apps/a"],
        "`only-a` binds only `a`, but holds the tree built for `both`.\n  both:   {first:?}\n  \
         only-a: {second:?}"
    );
}

/// The binding reaches a build without `{{bound…}}` too: a dropped
/// tracker's files are not checked out, so an install that finds its
/// members on disk, as an unfiltered `npm ci` in an npm workspace does,
/// builds less for an instance that binds less. This is why the binding is
/// in every resource's key, rather than only in those whose commands name
/// it.
#[test]
fn an_install_that_finds_its_members_on_disk_is_keyed_on_the_binding_too() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, DISCOVERING_INSTALL);

    let both = manager.spawn("both", None).expect("spawn both");
    assert_eq!(
        members(&both.branch.workspace_path),
        ["a", "b"],
        "precondition: both checked out"
    );
    let first = deps(&both.resources).install.clone();

    let only_a = manager
        .spawn_binding("only-a", None, &drop_b())
        .expect("spawn only-a");
    let second = deps(&only_a.resources).install.clone();

    assert_eq!(
        members(&only_a.branch.workspace_path),
        ["a"],
        "`only-a` has only `a` checked out, but holds the tree built for `both`.\n  both:   \
         {first:?}\n  only-a: {second:?}"
    );
}

/// Keying on the binding costs only the hits it has to: instances that bind
/// the same trackers build the same tree, so the second is filled from the
/// first, whatever else differs between them.
#[test]
fn instances_that_bind_the_same_trackers_still_share_one_tree() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, BOUND_INSTALL);

    let first = manager
        .spawn_binding("one", None, &drop_b())
        .expect("spawn one");
    let stored = match &deps(&first.resources).install {
        Some(InstallReport::Stored { key, .. }) => key.clone(),
        other => panic!("the first instance of a binding should store its tree: {other:?}"),
    };

    let second = manager
        .spawn_binding("two", None, &drop_b())
        .expect("spawn two");
    let report = deps(&second.resources);
    assert!(
        report.prepare.is_none(),
        "the second instance with that binding must not run the install"
    );
    match &report.install {
        Some(InstallReport::Filled { key, .. }) => {
            assert_eq!(key, &stored, "from the entry the first instance stored")
        }
        other => panic!("the second instance should be filled: {other:?}"),
    }
    assert_eq!(members(&second.branch.workspace_path), ["apps/a"]);
}

/// `newgit cleanup` keeps an entry while a live instance's key reaches it,
/// and recomputes those keys to decide, so it has to key each instance on
/// its own binding as spawn did. Otherwise the entry of an instance that
/// drops a tracker would look like garbage while that instance is live.
#[test]
fn cleanup_keeps_the_entry_each_live_binding_keys_to() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, BOUND_INSTALL);
    manager.spawn("both", None).expect("spawn both");
    manager
        .spawn_binding("only-a", None, &drop_b())
        .expect("spawn only-a");

    let outcome = manager
        .cleanup(true, ArchivedCheckpoints::Keep)
        .expect("dry run");
    assert!(
        outcome.pruned_installs.is_empty(),
        "both entries have a live instance keying to them: {:?}",
        outcome.pruned_installs
    );
}
