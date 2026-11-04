//! Tracker paths are checked by where they lead, not by how they are
//! spelled: each is normalized first, so `./.git` is `.git`, and `.git` and
//! `.newgit` are refused in any case. Inside `.newgit/`, a code tracker may
//! own other trackers' definitions, `.newgit/trackers/<name>.toml`: the
//! governance layering docs/design/trackers.md documents under "Who may change
//! what", and the one the remote resolves (`loadDefinitions` in
//! remote/src/index.ts). Nothing else in `.newgit/` opens up.
//!
//! `TrackerDefinition::validate` used to read a path's first component as
//! written, so `./.git`, which leads with `.`, got past the guard that
//! refuses `.git`, and the same guard refused every definition a tracker
//! claimed, making the project unopenable.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::{Storage, TrackerDefinition, collect_owned_files};

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

#[test]
fn a_leading_dot_slash_does_not_get_a_tracker_path_into_git() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let m = BranchManager::open(store).expect("manager");
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create tracker");

    // The guard, doing its job on the plain spelling.
    let plain = m
        .track_paths("runtime-env", &[Utf8PathBuf::from(".git")])
        .expect_err("`.git` is refused");
    assert!(plain.to_string().contains("may not reach into"), "{plain}");

    // The same directory, one `./` away.
    let dotted = m.track_paths("runtime-env", &[Utf8PathBuf::from("./.git")]);

    // What the tracker claims afterwards, and so what a capture would walk.
    let reopened = BranchManager::open(MetadataStore::at(root.clone())).expect("reopen");
    let definition = reopened
        .tracker_definitions()
        .iter()
        .find(|definition| definition.name == "runtime-env")
        .expect("runtime-env is defined");
    let swept = collect_owned_files(&root, definition).expect("collect owned files");

    assert!(
        dotted.is_err(),
        "`./.git` was accepted where `.git` is refused: {dotted:?}\n`runtime-env` now claims \
         {:?}, which in the store covers {} files of its Git directory, starting with {:?}",
        definition.paths,
        swept.len(),
        swept
            .iter()
            .take(3)
            .map(|(relative, _)| relative.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn an_admin_tracker_may_own_another_trackers_definition() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let trackers = store.paths().trackers.clone();

    // A project code tracker on its own opens fine.
    std::fs::write(
        trackers.join("web.toml"),
        r#"audience = "project-devs"
storage = "local"
secret = false
paths = ["apps/web"]
"#,
    )
    .expect("write web");
    BranchManager::open(MetadataStore::at(root.clone()))
        .expect("precondition: a plain code tracker definition opens");

    // The next layer: a tracker owning `web`'s definition, maintained by the
    // project's owner, so only they may change who can read `web`.
    std::fs::write(
        trackers.join("admin.toml"),
        r#"audience = "project-devs"
storage = "local"
secret = false
maintainers = "owner"
paths = [".newgit/trackers/web.toml"]
"#,
    )
    .expect("write admin");

    if let Err(error) = BranchManager::open(MetadataStore::at(root)) {
        panic!(
            "a code tracker owning another tracker's definition — the governance layering of \
             docs/design/trackers.md's \"Who may change what\", which the remote resolves — makes \
             the project unopenable: {error}"
        );
    }
}

/// `./.env.local` used to be stored as written, with the ignore line
/// `/./.env.local`, which Git matches to no file: the secret stayed in reach
/// of `git add .`.
#[test]
fn a_tracked_path_is_stored_and_ignored_as_the_path_it_names() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    std::fs::write(root.join(".env.local"), "SECRET=1\n").expect("write env");
    let m = BranchManager::open(store).expect("manager");
    m.create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create tracker");

    let outcome = m
        .track_paths("runtime-env", &[Utf8PathBuf::from("./.env.local")])
        .expect("track");
    assert_eq!(outcome.added_paths, [Utf8PathBuf::from(".env.local")]);
    assert_eq!(outcome.ignored_patterns, ["/.env.local"]);
    let reopened = BranchManager::open(MetadataStore::at(root.clone())).expect("reopen");
    let definition = reopened
        .tracker_definitions()
        .iter()
        .find(|definition| definition.name == "runtime-env")
        .expect("runtime-env is defined");
    assert_eq!(definition.paths, [Utf8PathBuf::from(".env.local")]);
    // `check-ignore` succeeds only for a path Git ignores.
    git(&root, &["check-ignore", "-q", ".env.local"]);
}

/// A definition is the one thing in `.newgit/` a tracker may claim, however
/// it is spelled, and only a code tracker may: a secret one would keep the
/// definition out of Git. The rest of `.newgit/` stays `project`'s and
/// `access`'s, and `.git` stays closed in any case, since a case-insensitive
/// file system, macOS's default, opens `.GIT` as the Git directory.
#[test]
fn only_a_code_tracker_may_own_a_definition_and_nothing_else_in_newgit_opens() {
    let code = |path: &str| {
        TrackerDefinition::code(
            "admin",
            "project-devs".to_owned(),
            &[Utf8PathBuf::from(path)],
        )
    };
    for spelling in [
        ".newgit/trackers/web.toml",
        "./.newgit/trackers/web.toml",
        ".newgit//trackers/./web.toml",
    ] {
        let admin = code(spelling).unwrap_or_else(|error| panic!("`{spelling}`: {error}"));
        assert_eq!(
            admin.paths,
            [Utf8PathBuf::from(".newgit/trackers/web.toml")]
        );
    }

    let error = TrackerDefinition::new(
        "admin",
        "project-devs".to_owned(),
        Storage::Local,
        false,
        vec![Utf8PathBuf::from(".newgit/trackers/web.toml")],
    )
    .expect_err("a secret tracker may not own a definition");
    assert!(error.to_string().contains("only a code tracker"), "{error}");

    for path in [
        ".newgit",
        ".newgit/trackers",
        "./.newgit/trackers/",
        ".newgit/trackers/README.md",
        ".newgit/trackers/nested/web.toml",
        ".newgit/resources/postgres.toml",
        ".newgit/config.toml",
        ".NEWGIT/trackers/web.toml",
        ".GIT",
        "./.Git/hooks",
    ] {
        let error = code(path).expect_err(path);
        assert!(
            error.to_string().contains("may not reach into"),
            "`{path}`: {error}"
        );
    }
}

/// What `newgit tracker create admin --code --path .newgit/trackers/web.toml`
/// runs. A code tracker used to be checked while it was still a secret one,
/// the default, so it could not claim a definition even with the check
/// fixed.
#[test]
fn an_admin_tracker_is_created_and_composed_like_any_code_tracker() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    BranchManager::open(store)
        .expect("manager")
        .create_code_tracker("web", "project-devs", &[Utf8PathBuf::from("apps/web")])
        .expect("create web");
    let outcome = BranchManager::open(MetadataStore::at(root.clone()))
        .expect("reopen")
        .create_code_tracker(
            "admin",
            "project-devs",
            &[Utf8PathBuf::from(".newgit/trackers/web.toml")],
        )
        .expect("create admin");
    assert_eq!(outcome.source, "access", "carved out of the file's owner");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "web, governed by admin"]);

    let m = BranchManager::open(MetadataStore::at(root)).expect("reopen");
    let access = m
        .code_trackers()
        .iter()
        .find(|definition| definition.name == "access")
        .expect("access is a code tracker");
    assert_eq!(
        access.excluded,
        [Utf8PathBuf::from(".newgit/trackers/web.toml")]
    );
    // With `access` no longer holding the file, only `admin`'s history can
    // put it in a workspace.
    let spawned = m.spawn("feature-a", None).expect("spawn");
    assert!(
        spawned
            .branch
            .workspace_path
            .join(".newgit/trackers/web.toml")
            .is_file(),
        "`web`'s definition is composed from `admin`"
    );
}
