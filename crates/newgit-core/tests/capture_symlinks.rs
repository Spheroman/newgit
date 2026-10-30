//! Capture never follows a symlink (#258). A tracker lane holds this
//! workspace's files and syncs wherever its audience reaches, so a link at a
//! tracked path that leads out of the workspace must not turn capture into
//! "copy any file I can read into the lane". A link that stays inside is kept
//! as a link (mode `120000`), as Git keeps one, and checkout writes it back
//! as a link — never one that leaves the workspace, whoever wrote the lane.
#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::error::NewgitError;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

const SECRET: &str = "OUTSIDE_SECRET=leak_9a7e\n";

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    git_stdin(dir, args, None)
}

fn git_stdin(dir: &Utf8Path, args: &[&str], stdin: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg(format!("--git-dir={dir}"))
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    if let Some(bytes) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(bytes)
            .expect("write stdin");
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("git exits");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A project with an `env` tracker owning `paths`, and one spawned instance.
/// Returns the manager, the instance's workspace, and a directory outside
/// it holding a secret (`outside/secret.txt`, `outside/dir/secret.txt`).
fn setup(temp: &Utf8Path, paths: &[&str]) -> (BranchManager, Utf8PathBuf, Utf8PathBuf) {
    let repo = temp.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo.as_str())
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    };
    run(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");

    let manager = BranchManager::open(store).expect("manager");
    manager
        .create_tracker("env", "project-devs", Storage::Local, false)
        .expect("create tracker");
    let paths: Vec<Utf8PathBuf> = paths.iter().map(Utf8PathBuf::from).collect();
    manager.track_paths("env", &paths).expect("track");
    let manager = BranchManager::open(manager.store().clone()).expect("reopen");

    let workspace = manager
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;

    let outside = temp.join("outside");
    std::fs::create_dir_all(outside.join("dir")).expect("mkdir outside");
    std::fs::write(outside.join("secret.txt"), SECRET).expect("write secret");
    std::fs::write(outside.join("dir/secret.txt"), SECRET).expect("write secret");
    (manager, workspace, outside)
}

fn tracker_store(manager: &BranchManager) -> Utf8PathBuf {
    manager.store().paths().tracker_store.clone()
}

/// Whether any object in the tracker store holds the secret.
fn store_leaks(manager: &BranchManager) -> bool {
    let store = tracker_store(manager);
    if !store.join("HEAD").is_file() {
        return false;
    }
    let objects = git(
        &store,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    objects
        .lines()
        .any(|object| git(&store, &["cat-file", "-p", object]).contains("leak_9a7e"))
}

fn assert_refused(error: &NewgitError, path: &str, target: &str) {
    let message = error.to_string();
    assert!(
        message.contains(&format!("`{path}`")) && message.contains(target),
        "the refusal names the path and where it leads: {message}"
    );
}

#[test]
fn a_file_symlink_leaving_the_workspace_is_refused() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, &[".env.local"]);
    let target = outside.join("secret.txt");
    symlink(&target, workspace.join(".env.local")).expect("symlink");

    let error = manager
        .capture_tracker("feature", "env")
        .expect_err("capture must not follow a link out of the workspace");
    assert!(
        matches!(error, NewgitError::SymlinkLeavesWorkspace { .. }),
        "{error:?}"
    );
    assert_refused(&error, ".env.local", target.as_str());

    let error = manager
        .checkpoint("feature", None)
        .expect_err("nor may the capture inside a checkpoint");
    assert!(
        error.to_string().contains(".env.local"),
        "the checkpoint says which path: {error}"
    );
    assert!(!store_leaks(&manager), "the secret never entered the store");
}

#[test]
fn a_relative_link_that_climbs_out_is_refused() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, _outside) = setup(&temp, &[".env.local"]);
    // From the workspace (temp/workspaces/<name>), `../../outside/...`.
    symlink("../../outside/secret.txt", workspace.join(".env.local")).expect("symlink");

    let error = manager
        .capture_tracker("feature", "env")
        .expect_err("a relative target can still leave");
    assert_refused(&error, ".env.local", "../../outside/secret.txt");
    assert!(!store_leaks(&manager));
}

#[test]
fn a_link_that_leaves_through_another_link_is_refused() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, &[".env.local"]);
    // `.env.local -> door/secret.txt` reads as inside, but `door` (not
    // tracked) leads out: resolved on disk, it is outside.
    symlink(&outside, workspace.join("door")).expect("symlink door");
    symlink("door/secret.txt", workspace.join(".env.local")).expect("symlink");

    let error = manager
        .capture_tracker("feature", "env")
        .expect_err("resolved, the link leaves");
    assert_refused(&error, ".env.local", "door/secret.txt");
    assert!(!store_leaks(&manager));
}

#[test]
fn a_directory_symlink_under_a_tracked_directory_is_refused() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, &["config"]);
    std::fs::create_dir_all(workspace.join("config")).expect("mkdir");
    std::fs::write(workspace.join("config/app.env"), "A=1\n").expect("write");
    symlink(outside.join("dir"), workspace.join("config/ext")).expect("symlink");

    let error = manager
        .capture_tracker("feature", "env")
        .expect_err("a linked directory is not walked into");
    assert_refused(&error, "config/ext", outside.join("dir").as_str());
    assert!(!store_leaks(&manager));
}

#[test]
fn a_symlinked_parent_of_a_tracked_path_is_refused() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, &["config/secrets.env"]);
    std::fs::write(outside.join("dir/secrets.env"), SECRET).expect("write");
    // `config` is not the tracker's, but the tracked file is "under" it.
    symlink(outside.join("dir"), workspace.join("config")).expect("symlink");

    let error = manager
        .capture_tracker("feature", "env")
        .expect_err("the path is not in the workspace");
    assert!(
        matches!(error, NewgitError::SymlinkedParent { .. }),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("`config/secrets.env`")
            && message.contains("`config`")
            && message.contains(outside.join("dir").as_str()),
        "names the path, the link, and its target: {message}"
    );
    assert!(!store_leaks(&manager));
}

#[test]
fn a_link_inside_the_workspace_is_kept_as_a_link_and_checked_out_as_one() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, _outside) = setup(&temp, &[".env.local", "config"]);
    std::fs::write(workspace.join(".env.shared"), "SHARED=1\n").expect("write");
    symlink(".env.shared", workspace.join(".env.local")).expect("symlink");
    std::fs::create_dir_all(workspace.join("config/real")).expect("mkdir");
    std::fs::write(workspace.join("config/real/a.env"), "A=1\n").expect("write");
    // A directory link inside: stored as a link, not walked (no duplicate
    // `config/alias/a.env`).
    symlink("real", workspace.join("config/alias")).expect("symlink dir");

    let report = manager
        .capture_tracker("feature", "env")
        .expect("an in-workspace link is captured");
    let store = tracker_store(&manager);
    let commit = git(
        &store,
        &[
            "rev-parse",
            &format!("refs/trackers/env/pins/{}", report.rev),
        ],
    );
    let tree = git(&store, &["ls-tree", "-r", &commit]);
    let line = |path: &str| {
        tree.lines()
            .find(|line| line.ends_with(&format!("\t{path}")))
            .unwrap_or_else(|| panic!("{path} in tree:\n{tree}"))
            .to_owned()
    };
    assert!(line(".env.local").starts_with("120000 "), "{tree}");
    assert!(line("config/alias").starts_with("120000 "), "{tree}");
    assert!(line("config/real/a.env").starts_with("100644 "), "{tree}");
    assert!(
        !tree.contains("config/alias/"),
        "a linked directory is not descended into: {tree}"
    );
    assert_eq!(
        git(&store, &["cat-file", "-p", &format!("{commit}:.env.local")]),
        ".env.shared",
        "a link's blob is its target, not the target's content"
    );

    // Round trip: wipe, check out, and the links come back as links.
    std::fs::remove_file(workspace.join(".env.local")).expect("rm");
    std::fs::remove_dir_all(workspace.join("config")).expect("rm");
    manager
        .checkout_tracker("feature", "env", Some(&report.rev))
        .expect("checkout");
    let meta = std::fs::symlink_metadata(workspace.join(".env.local")).expect("lstat");
    assert!(meta.is_symlink(), "checked out as a link, not a copy");
    assert_eq!(
        std::fs::read_link(workspace.join(".env.local")).expect("readlink"),
        std::path::Path::new(".env.shared")
    );
    assert!(
        std::fs::symlink_metadata(workspace.join("config/alias"))
            .expect("lstat")
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("config/alias/a.env")).expect("read"),
        "A=1\n"
    );

    // Capturing again is the same content: no new commit.
    let again = manager.capture_tracker("feature", "env").expect("again");
    assert!(!again.changed, "the round trip is exact");
}

#[test]
fn a_lane_link_leading_out_is_never_checked_out() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, &[".env.local"]);
    std::fs::write(workspace.join(".env.local"), "MINE=1\n").expect("write");
    manager.capture_tracker("feature", "env").expect("capture");

    // Someone else's lane (or a remote's) holds `.env.local -> <outside>`:
    // built straight in the store, as a fetch would deliver it.
    let store = tracker_store(&manager);
    let target = outside.join("secret.txt");
    let blob = git_stdin(
        &store,
        &["hash-object", "-w", "--stdin"],
        Some(target.as_str().as_bytes()),
    );
    let tree = git_stdin(
        &store,
        &["mktree"],
        Some(format!("120000 blob {blob}\t.env.local\n").as_bytes()),
    );
    let commit = git(&store, &["commit-tree", &tree, "-m", "planted"]);
    let rev = &commit[..12];
    git(
        &store,
        &[
            "update-ref",
            &format!("refs/trackers/env/pins/{rev}"),
            &commit,
        ],
    );

    let error = manager
        .checkout_tracker("feature", "env", Some(rev))
        .expect_err("a link out of the workspace is refused");
    assert!(
        matches!(error, NewgitError::LaneLinkLeavesWorkspace { .. }),
        "{error:?}"
    );
    assert_refused(&error, ".env.local", target.as_str());
    let meta = std::fs::symlink_metadata(workspace.join(".env.local")).expect("lstat");
    assert!(
        meta.is_file() && !meta.is_symlink(),
        "the workspace is untouched by a refused checkout"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.local")).expect("read"),
        "MINE=1\n"
    );
}

#[test]
fn checkout_never_writes_through_a_symlinked_parent() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, &["config/secrets.env"]);
    std::fs::create_dir_all(workspace.join("config")).expect("mkdir");
    std::fs::write(workspace.join("config/secrets.env"), "MINE=1\n").expect("write");
    let captured = manager.capture_tracker("feature", "env").expect("capture");

    // `config` becomes a link out, then the lane is checked out on top. The
    // safety capture refuses first, so drive the lane directly too.
    std::fs::remove_dir_all(workspace.join("config")).expect("rm");
    symlink(outside.join("dir"), workspace.join("config")).expect("symlink");
    assert!(
        manager
            .checkout_tracker("feature", "env", Some(&captured.rev))
            .is_err(),
        "the safety capture refuses the symlinked parent"
    );

    let paths = manager.store().paths();
    let lane = newgit_core::tracker_repo::TrackerRepo::new(
        &paths.tracker_store,
        &paths.tracker_checkouts,
        "env",
    );
    let definition = manager
        .tracker_definitions()
        .iter()
        .find(|definition| definition.name == "env")
        .expect("env")
        .clone();
    // Git's own checkout replaces a symlinked leading directory with a real
    // one rather than writing through it.
    lane.restore(&workspace, &definition, &captured.rev)
        .expect("restore");
    assert!(
        std::fs::symlink_metadata(workspace.join("config"))
            .expect("lstat")
            .is_dir(),
        "the linked parent became a real directory"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("config/secrets.env")).expect("read"),
        "MINE=1\n"
    );
    assert!(
        !outside.join("dir/secrets.env").exists(),
        "nothing was written through the link"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("dir/secret.txt")).expect("read"),
        SECRET,
        "nothing behind the link was removed or changed"
    );
}
