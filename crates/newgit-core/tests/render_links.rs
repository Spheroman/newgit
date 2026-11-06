//! A render never writes through a link (#283).
//!
//! A render writes this instance's ports and exports into its file. Through
//! a symlink at the path, or at a directory above it, that write would land
//! wherever the link leads: a file outside the workspace, or another file
//! inside it. So a render refuses, naming the link and its target, and
//! taking the values back out (before a capture, or when a resource is
//! unbound) leaves such a file alone rather than writing through the link.
//! A hard link looks like any regular file, so each of those writes judges
//! the file it opened: one with more than one name is replaced rather than
//! written into, and the link's other names keep what they held, while one
//! with a single name is still rewritten in place.
#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::Command;
use std::time::{Duration, SystemTime};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::ResourceStatus;
use newgit_core::config::WorkspaceSection;
use newgit_core::error::NewgitError;
use newgit_core::manager::{BranchManager, ResourceBindOutcome, UndoOptions};
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

const CONFIG: &str = "API_URL=http://127.0.0.1:54321\n";
const DB: &str = "DB_URL=postgres://127.0.0.1:54322\n";
const MINE: &str = "MINE=1\n";

/// `web` renders its port into `config/app.env`.
const WEB: &str = r#"ownership = "branch"

[ports]
api = { start = 48931 }

[[render]]
path = "config/app.env"
replace = [{ find = "127.0.0.1:54321", with = "127.0.0.1:{{ports.api}}" }]
"#;

/// As [`WEB`], for the file the secret tracker `env` owns.
const WEB_TRACKED: &str = r#"ownership = "branch"
depends_on = ["env"]

[ports]
api = { start = 48931 }

[[render]]
path = "config/app.env"
replace = [{ find = "127.0.0.1:54321", with = "127.0.0.1:{{ports.api}}" }]
"#;

/// As [`WEB`], and its database port into `db.env` too.
const WEB_TWO_FILES: &str = r#"ownership = "branch"

[ports]
api = { start = 48931 }
db = { start = 48941 }

[[render]]
path = "config/app.env"
replace = [{ find = "127.0.0.1:54321", with = "127.0.0.1:{{ports.api}}" }]

[[render]]
path = "db.env"
replace = [{ find = "127.0.0.1:54322", with = "127.0.0.1:{{ports.db}}" }]
"#;

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

/// Who holds `config/app.env`'s committed content.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// The project repository.
    Source,
    /// The secret tracker `env`.
    Tracker,
}

/// A project with `db.env` committed and `config/app.env` committed where
/// `owner` says, the resource `web` defined by `resource`, and one spawned
/// instance `x` that rendered it. Returns the manager, the workspace, and a
/// directory outside it that holds `target.env` and `config/app.env`, both
/// reading [`MINE`].
fn setup(
    temp: &Utf8Path,
    owner: Owner,
    resource: &str,
) -> (BranchManager, Utf8PathBuf, Utf8PathBuf) {
    let repo = temp.join("repo");
    std::fs::create_dir_all(repo.join("config")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("db.env"), DB).expect("write");
    if owner == Owner::Source {
        std::fs::write(repo.join("config/app.env"), CONFIG).expect("write");
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    if owner == Owner::Tracker {
        std::fs::write(repo.join("config/app.env"), CONFIG).expect("write");
    }

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
        .write_resource_file("web", resource)
        .expect("resource");

    let mut manager = BranchManager::open(store).expect("manager");
    if owner == Owner::Tracker {
        manager
            .create_tracker("env", "user", Storage::Local, false)
            .expect("create tracker");
        manager
            .track_paths("env", &[Utf8PathBuf::from("config/app.env")])
            .expect("track");
        manager.seed_tracker_from_store("env").expect("seed");
        manager = BranchManager::open(MetadataStore::at(
            manager.store().paths().project_root.clone(),
        ))
        .expect("reopen");
    }

    let spawned = manager.spawn("x", None).expect("spawn");
    let web = spawned
        .resources
        .iter()
        .find(|outcome| outcome.name == "web")
        .expect("web");
    assert!(web.render_error.is_none(), "{:?}", web.render_error);
    let workspace = spawned.branch.workspace_path.clone();

    let outside = temp.join("outside");
    std::fs::create_dir_all(outside.join("config")).expect("mkdir outside");
    std::fs::write(outside.join("target.env"), MINE).expect("write");
    std::fs::write(outside.join("config/app.env"), MINE).expect("write");
    (manager, workspace, outside)
}

/// Bind `web` again, which renders it again.
fn rebind(manager: &BranchManager) -> ResourceBindOutcome {
    manager.rebind_resource("x", "web").expect("rebind").bound
}

/// Why the render failed, which it must have.
fn refusal(bound: &ResourceBindOutcome) -> &str {
    assert_eq!(bound.status, ResourceStatus::Failed);
    bound
        .render_error
        .as_deref()
        .expect("the render was refused")
}

fn read(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).expect("read")
}

/// A modification time no write made: anything that writes the file moves
/// it to now.
fn pinned() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
}

fn pin_mtime(path: &Utf8Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(pinned())
        .expect("set mtime");
}

fn mtime(path: &Utf8Path) -> SystemTime {
    std::fs::metadata(path)
        .expect("stat")
        .modified()
        .expect("mtime")
}

#[test]
fn a_rendered_file_replaced_by_a_link_out_of_the_workspace_is_refused() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB);
    let target = outside.join("target.env");
    std::fs::remove_file(workspace.join("config/app.env")).expect("rm");
    symlink(&target, workspace.join("config/app.env")).expect("symlink");

    let bound = rebind(&manager);
    assert_eq!(read(&target), MINE, "nothing was written through the link");
    let error = refusal(&bound);
    assert!(
        error.contains("`config/app.env`")
            && error.contains(&format!("is a symlink to `{target}`")),
        "the refusal names the path and where it leads: {error}"
    );
    assert!(
        std::fs::symlink_metadata(workspace.join("config/app.env"))
            .expect("lstat")
            .is_symlink(),
        "the link is left for the user to replace"
    );
}

#[test]
fn an_undo_does_not_re_render_through_a_link() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB);
    manager.checkpoint("x", Some("before")).expect("checkpoint");
    let target = outside.join("target.env");
    std::fs::remove_file(workspace.join("config/app.env")).expect("rm");
    symlink(&target, workspace.join("config/app.env")).expect("symlink");

    let undone = manager.undo("x", &UndoOptions::default()).expect("undo");
    assert_eq!(read(&target), MINE, "nothing was written through the link");
    assert!(
        undone.warnings.iter().any(|warning| {
            warning.contains("re-render for resource `web` failed")
                && warning.contains("`config/app.env`")
                && warning.contains(target.as_str())
        }),
        "the re-render says why it did not run: {:?}",
        undone.warnings
    );
    assert!(
        !undone
            .warnings
            .iter()
            .any(|warning| warning.contains("render will discard")),
        "nothing behind the link is discarded, so nothing says it is: {:?}",
        undone.warnings
    );
}

#[test]
fn a_link_that_stays_inside_the_workspace_is_refused_too() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, _outside) = setup(&temp, Owner::Source, WEB);
    // Through it, the render would rewrite `README.md` with `app.env`'s
    // committed content and this instance's port.
    std::fs::remove_file(workspace.join("config/app.env")).expect("rm");
    symlink("../README.md", workspace.join("config/app.env")).expect("symlink");

    let bound = rebind(&manager);
    assert_eq!(read(&workspace.join("README.md")), "hello\n");
    let error = refusal(&bound);
    assert!(
        error.contains("`config/app.env`") && error.contains("`../README.md`"),
        "{error}"
    );
}

#[test]
fn a_dangling_link_is_refused_and_nothing_is_created() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB);
    let target = outside.join("new.env");
    std::fs::remove_file(workspace.join("config/app.env")).expect("rm");
    symlink(&target, workspace.join("config/app.env")).expect("symlink");

    let bound = rebind(&manager);
    assert!(
        !target.exists(),
        "a write through a dangling link would have created its target"
    );
    let error = refusal(&bound);
    assert!(error.contains(target.as_str()), "{error}");
}

#[test]
fn a_rendered_file_under_a_symlinked_directory_is_refused() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB);
    let target = outside.join("config");
    std::fs::remove_dir_all(workspace.join("config")).expect("rm");
    symlink(&target, workspace.join("config")).expect("symlink");

    let bound = rebind(&manager);
    assert_eq!(read(&target.join("app.env")), MINE);
    let error = refusal(&bound);
    assert!(
        error.contains("`config/app.env`")
            && error.contains(&format!("`config` is a symlink to `{target}`")),
        "the refusal names the path, the link, and its target: {error}"
    );
}

#[test]
fn a_refused_render_writes_none_of_the_resources_files() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB_TWO_FILES);
    // The first file is fine and holds an edit; the second is a link.
    let edited = read(&workspace.join("config/app.env")) + "EDITED=1\n";
    std::fs::write(workspace.join("config/app.env"), &edited).expect("edit");
    std::fs::remove_file(workspace.join("db.env")).expect("rm");
    symlink(outside.join("target.env"), workspace.join("db.env")).expect("symlink");

    let bound = rebind(&manager);
    assert_eq!(read(&outside.join("target.env")), MINE);
    assert_eq!(
        read(&workspace.join("config/app.env")),
        edited,
        "every path is judged before any is written"
    );
    let error = refusal(&bound);
    assert!(error.contains("`db.env`"), "{error}");
}

#[test]
fn a_capture_never_unrenders_through_a_symlinked_directory() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Tracker, WEB_TRACKED);
    // The rendered file, port and all, now lives outside, reached through
    // a linked `config`.
    let moved = outside.join("moved");
    std::fs::rename(workspace.join("config"), &moved).expect("move");
    symlink(&moved, workspace.join("config")).expect("symlink");
    let rendered = read(&moved.join("app.env"));
    assert_ne!(rendered, CONFIG, "the file holds this instance's port");
    pin_mtime(&moved.join("app.env"));

    let error = manager
        .capture_tracker("x", "env")
        .expect_err("capture refuses a path under a symlinked directory");
    assert!(
        matches!(error, NewgitError::SymlinkedParent { .. }),
        "{error:?}"
    );
    assert_eq!(read(&moved.join("app.env")), rendered);
    assert_eq!(
        mtime(&moved.join("app.env")),
        pinned(),
        "the values were not taken out, and put back, through the link"
    );
}

#[test]
fn unbinding_never_unrenders_through_a_symlinked_directory() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB);
    let moved = outside.join("moved");
    std::fs::rename(workspace.join("config"), &moved).expect("move");
    symlink(&moved, workspace.join("config")).expect("symlink");
    let rendered = read(&moved.join("app.env"));
    assert_ne!(rendered, CONFIG, "the file holds this instance's port");

    manager.remove_resource("web", true).expect("remove");
    assert_eq!(
        read(&moved.join("app.env")),
        rendered,
        "unbinding does not take the values out through the link"
    );
}

/// How many names the file at `path` has.
fn links(path: &Utf8Path) -> u64 {
    std::fs::metadata(path).expect("stat").nlink()
}

/// Which file is at `path`: a new one in its place has another inode.
fn inode(path: &Utf8Path) -> u64 {
    std::fs::metadata(path).expect("stat").ino()
}

fn mode(path: &Utf8Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

/// What `dir` holds, by name.
fn names(dir: &Utf8Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_hard_link_at_a_rendered_path_is_replaced_not_written_through() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB);
    let target = outside.join("target.env");
    std::fs::remove_file(workspace.join("config/app.env")).expect("rm");
    std::fs::hard_link(&target, workspace.join("config/app.env")).expect("hard link");
    pin_mtime(&target);

    let bound = rebind(&manager);
    assert_eq!(read(&target), MINE, "nothing was written through the link");
    assert_eq!(mtime(&target), pinned(), "not even for a moment");
    assert!(bound.render_error.is_none(), "{:?}", bound.render_error);
    let rendered = read(&workspace.join("config/app.env"));
    assert!(
        rendered.starts_with("API_URL=http://127.0.0.1:") && rendered != CONFIG,
        "the workspace's path holds the render: {rendered}"
    );
    assert_eq!(links(&target), 1, "the path got a file of its own");
    assert_eq!(
        names(&workspace.join("config")),
        ["app.env"],
        "nothing is left beside it"
    );
}

#[test]
fn a_capture_never_unrenders_through_a_hard_link() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Tracker, WEB_TRACKED);
    // A second name for the rendered file, port and all, outside the
    // workspace.
    let other = outside.join("linked.env");
    std::fs::hard_link(workspace.join("config/app.env"), &other).expect("hard link");
    let rendered = read(&other);
    assert_ne!(rendered, CONFIG, "the file holds this instance's port");
    pin_mtime(&other);

    manager.capture_tracker("x", "env").expect("capture");
    assert_eq!(read(&other), rendered);
    assert_eq!(
        mtime(&other),
        pinned(),
        "the values were not taken out, and put back, through the link"
    );
    assert_eq!(
        read(&workspace.join("config/app.env")),
        rendered,
        "the workspace's file ends the capture as it began it"
    );
}

#[test]
fn unbinding_never_unrenders_through_a_hard_link() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB);
    let other = outside.join("linked.env");
    std::fs::hard_link(workspace.join("config/app.env"), &other).expect("hard link");
    let rendered = read(&other);
    assert_ne!(rendered, CONFIG, "the file holds this instance's port");

    manager.remove_resource("web", true).expect("remove");
    assert_eq!(
        read(&other),
        rendered,
        "unbinding does not take the values out through the link"
    );
    assert_eq!(read(&workspace.join("config/app.env")), CONFIG);
}

#[test]
fn a_hard_linked_file_keeps_its_mode_when_replaced() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, outside) = setup(&temp, Owner::Source, WEB);
    let path = workspace.join("config/app.env");
    std::fs::hard_link(&path, outside.join("linked.env")).expect("hard link");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");

    let bound = rebind(&manager);
    assert!(bound.render_error.is_none(), "{:?}", bound.render_error);
    assert_eq!(links(&path), 1, "replaced, not written into");
    assert_eq!(mode(&path), 0o640);
}

/// So a process holding it open, or a container that bind-mounts it alone,
/// sees what the render writes.
#[test]
fn a_file_with_one_name_is_rewritten_in_place() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, _outside) = setup(&temp, Owner::Source, WEB);
    let path = workspace.join("config/app.env");
    // Back to committed content, so the render has something to change.
    std::fs::write(&path, CONFIG).expect("write");
    let before = inode(&path);

    let bound = rebind(&manager);
    assert!(bound.render_error.is_none(), "{:?}", bound.render_error);
    assert_ne!(read(&path), CONFIG, "the render wrote it");
    assert_eq!(
        inode(&path),
        before,
        "the same file, not a new one in its place"
    );
}

#[test]
fn a_capture_leaves_a_file_with_one_name_in_place() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, _outside) = setup(&temp, Owner::Tracker, WEB_TRACKED);
    let path = workspace.join("config/app.env");
    let rendered = read(&path);
    let before = inode(&path);

    manager.capture_tracker("x", "env").expect("capture");
    assert_eq!(read(&path), rendered);
    assert_eq!(
        inode(&path),
        before,
        "the values were taken out and put back in the same file"
    );
}

#[test]
fn a_file_the_render_creates_gets_the_mode_a_plain_write_would() {
    let (_guard, temp) = tempdir();
    let (manager, workspace, _outside) = setup(&temp, Owner::Source, WEB);
    let path = workspace.join("config/app.env");
    std::fs::remove_file(&path).expect("rm");
    let probe = temp.join("probe");
    std::fs::write(&probe, "").expect("write");

    let bound = rebind(&manager);
    assert!(bound.render_error.is_none(), "{:?}", bound.render_error);
    assert!(read(&path).starts_with("API_URL="), "the render wrote it");
    assert_eq!(
        mode(&path),
        mode(&probe),
        "0666 less the umask, as `std::fs::write` gives"
    );
}
