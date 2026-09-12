use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::export::{ExportFilter, Reason};
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;
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

fn git_stdout(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").expect("write");
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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A store with a public `generated-sdk` lane and a user-scoped
/// `runtime-env` lane, one instance spawned, and content for both on disk.
fn project_with_two_lanes(temp: &Utf8Path) -> (BranchManager, Utf8PathBuf) {
    let store = setup(temp);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("manager");

    manager
        .create_tracker("generated-sdk", "public", Storage::Local, true)
        .expect("create public tracker");
    manager
        .track_paths("generated-sdk", &[Utf8PathBuf::from("src/generated")])
        .expect("track");
    manager
        .create_tracker("runtime-env", "user", Storage::Local, false)
        .expect("create user tracker");
    manager
        .track_paths("runtime-env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");

    // Reopen so the manager sees both definitions, then spawn.
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let spawned = manager.spawn("auth-refactor", None).expect("spawn");
    let workspace = spawned.branch.workspace_path.clone();

    std::fs::create_dir_all(workspace.join("src/generated")).expect("mkdir");
    std::fs::write(workspace.join("src/generated/api.ts"), "export {};\n").expect("write");
    std::fs::write(workspace.join(".env.local"), "SECRET=hunter2\n").expect("write");

    (manager, workspace)
}

#[test]
fn export_ships_source_and_public_lanes_and_withholds_the_rest() {
    let (_guard, temp) = tempdir();
    let (manager, _workspace) = project_with_two_lanes(&temp);
    let destination = temp.join("public-export");

    let outcome = manager
        .export("auth-refactor", &destination, &ExportFilter::default())
        .expect("export");

    // Source ships (its audience is everyone) and so does the public lane.
    assert!(destination.join("README.md").is_file());
    assert!(destination.join("src/main.rs").is_file());
    assert!(destination.join("src/generated/api.ts").is_file());
    assert_eq!(outcome.plan.count(Reason::PublicTracker), 1);

    // The user-scoped lane does not, and says so.
    assert!(
        !destination.join(".env.local").exists(),
        "a user-audience lane must not land in an export"
    );
    let env = outcome
        .plan
        .trackers
        .iter()
        .find(|tracker| tracker.name == "runtime-env")
        .expect("runtime-env disposition");
    assert_eq!(env.withheld, [Utf8PathBuf::from(".env.local")]);
    assert!(!env.is_public());

    // The result is an ordinary Git repository: one commit, right branch,
    // and no newgit remote or ref namespace.
    assert_eq!(
        git_stdout(&destination, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "auth-refactor"
    );
    assert_eq!(
        git_stdout(&destination, &["rev-list", "--count", "HEAD"]),
        "1"
    );
    assert_eq!(git_stdout(&destination, &["remote"]), "");
    assert!(
        !git_stdout(&destination, &["for-each-ref", "--format=%(refname)"]).contains("newgit"),
        "an export must not carry newgit's ref namespace"
    );
    assert!(
        git_stdout(&destination, &["status", "--porcelain"]).is_empty(),
        "everything exported should be committed"
    );
    assert_eq!(
        git_stdout(&destination, &["rev-parse", "HEAD"]),
        outcome.commit
    );
}

#[test]
fn export_includes_uncommitted_source_edits_but_not_ignored_files() {
    let (_guard, temp) = tempdir();
    let (manager, workspace) = project_with_two_lanes(&temp);
    let destination = temp.join("export");

    // An agent's uncommitted work is part of the workspace, so it exports.
    std::fs::write(workspace.join("src/main.rs"), "fn main() { work(); }\n").expect("write");
    // Ignored build junk is not.
    std::fs::create_dir_all(workspace.join("node_modules/pkg")).expect("mkdir");
    std::fs::write(workspace.join("node_modules/pkg/index.js"), "junk\n").expect("write");
    std::fs::write(workspace.join(".gitignore"), "/node_modules/\n").expect("write");

    manager
        .export("auth-refactor", &destination, &ExportFilter::default())
        .expect("export");

    assert_eq!(
        std::fs::read_to_string(destination.join("src/main.rs")).expect("read"),
        "fn main() { work(); }\n"
    );
    assert!(!destination.join("node_modules").exists());
}

#[test]
fn include_overrides_audience_and_exclude_wins_over_everything() {
    let (_guard, temp) = tempdir();
    let (manager, _workspace) = project_with_two_lanes(&temp);

    let forced = temp.join("with-env");
    let outcome = manager
        .export(
            "auth-refactor",
            &forced,
            &ExportFilter {
                includes: vec![Utf8PathBuf::from(".env.local")],
                excludes: Vec::new(),
            },
        )
        .expect("export");
    assert!(
        forced.join(".env.local").is_file(),
        "--include overrides audience"
    );
    assert_eq!(outcome.plan.count(Reason::Forced), 1);

    let trimmed = temp.join("without-readme");
    let outcome = manager
        .export(
            "auth-refactor",
            &trimmed,
            &ExportFilter {
                includes: vec![Utf8PathBuf::from(".env.local")],
                excludes: vec![
                    Utf8PathBuf::from("README.md"),
                    Utf8PathBuf::from(".env.local"),
                ],
            },
        )
        .expect("export");
    assert!(
        !trimmed.join("README.md").exists(),
        "--exclude drops source"
    );
    assert!(
        !trimmed.join(".env.local").exists(),
        "--exclude beats --include"
    );
    assert_eq!(outcome.plan.count(Reason::Forced), 0);
}

#[test]
fn export_refuses_a_nonempty_destination_and_an_empty_result() {
    let (_guard, temp) = tempdir();
    let (manager, _workspace) = project_with_two_lanes(&temp);

    let occupied = temp.join("occupied");
    std::fs::create_dir_all(&occupied).expect("mkdir");
    std::fs::write(occupied.join("keep.txt"), "mine\n").expect("write");
    assert!(matches!(
        manager.export("auth-refactor", &occupied, &ExportFilter::default()),
        Err(NewgitError::Unsupported(_))
    ));
    assert!(
        occupied.join("keep.txt").is_file(),
        "a refused export must not touch the destination"
    );

    // Excluding everything is a mistake worth an error, not an empty repo.
    let empty = temp.join("empty-result");
    assert!(matches!(
        manager.export(
            "auth-refactor",
            &empty,
            &ExportFilter {
                includes: Vec::new(),
                excludes: vec![Utf8PathBuf::from("")],
            },
        ),
        Err(NewgitError::Unsupported(_))
    ));
    assert!(!empty.join(".git").exists(), "no repository on refusal");
}
