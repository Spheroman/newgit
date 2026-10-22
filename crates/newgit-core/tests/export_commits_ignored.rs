//! An export commits every path its plan ships, even one the project's
//! `.gitignore` matches (#94). `tracker track` gitignores a lane's paths and
//! that `.gitignore` ships with the source tree, so a plain `git add -A` in
//! the export dropped every tracker path — `--include`d or public — onto disk
//! and out of the repository.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::export::ExportFilter;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn ls_files(dir: &Utf8Path) -> Vec<String> {
    git(dir, &["ls-files"]).lines().map(str::to_owned).collect()
}

/// A project with a public lane and a user lane whose `.gitignore` block is
/// *committed* before the instance is spawned — the real shape, and the one
/// that puts the tracker patterns into the exported tree.
fn project(temp: &Utf8Path) -> (BranchManager, Utf8PathBuf) {
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
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let workspace = manager
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    std::fs::create_dir_all(workspace.join("src/generated")).expect("mkdir");
    std::fs::write(workspace.join("src/generated/api.ts"), "export {};\n").expect("write");
    std::fs::write(workspace.join(".env.local"), "SECRET=hunter2\n").expect("write");
    (manager, workspace)
}

#[test]
fn included_and_public_tracker_paths_are_committed_not_just_written() {
    let temp = tempfile::tempdir().expect("tempdir");
    let temp = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8");
    let (manager, workspace) = project(&temp);

    // The precondition the bug needs: the shipped `.gitignore` covers both.
    let gitignore = std::fs::read_to_string(workspace.join(".gitignore")).expect("read");
    assert!(gitignore.contains(".env.local") && gitignore.contains("src/generated"));

    let destination = temp.join("export");
    let outcome = manager
        .export(
            "feature",
            &destination,
            &ExportFilter {
                includes: vec![Utf8PathBuf::from(".env.local")],
                excludes: Vec::new(),
            },
        )
        .expect("export");

    let tracked = ls_files(&destination);
    assert!(
        tracked.contains(&".env.local".to_owned()),
        "--include ships the path in the repository, not just the directory: {tracked:?}"
    );
    assert!(
        tracked.contains(&"src/generated/api.ts".to_owned()),
        "a public lane ships in the repository too: {tracked:?}"
    );

    // Every file on disk is a file in the commit, so a clone is the export.
    let clone = temp.join("clone");
    git(
        &temp,
        &["clone", "-q", destination.as_str(), clone.as_str()],
    );
    assert_eq!(
        std::fs::read_to_string(clone.join(".env.local")).expect("cloned"),
        "SECRET=hunter2\n"
    );

    // The `.gitignore` ships as it is, and the outcome names what it still
    // matches so the CLI can say so.
    assert_eq!(
        outcome.still_gitignored,
        [
            Utf8PathBuf::from(".env.local"),
            Utf8PathBuf::from("src/generated/api.ts")
        ]
    );
    assert!(git(&destination, &["status", "--porcelain", "--ignored"]).is_empty());
}

#[test]
fn a_withheld_path_still_does_not_reach_the_repository() {
    let temp = tempfile::tempdir().expect("tempdir");
    let temp = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8");
    let (manager, _workspace) = project(&temp);

    let destination = temp.join("export");
    manager
        .export("feature", &destination, &ExportFilter::default())
        .expect("export");

    assert!(!destination.join(".env.local").exists());
    assert!(!ls_files(&destination).contains(&".env.local".to_owned()));
    // `--force` must not sweep in anything the plan did not place.
    assert!(git(&destination, &["status", "--porcelain", "--ignored"]).is_empty());
}
