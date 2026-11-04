//! A checkpoint snapshots uncommitted work with `git add -A` into a
//! throwaway index. A dropped code tracker's paths are outside the
//! workspace's sparse checkout, where `git add -A` skips an edited file
//! without a word and fails on a new one. So a file written into a dropped
//! tracker's directory made `checkpoint` fail, and `undo` with it, through
//! the checkpoint it takes first. An edit to one of that tracker's files was
//! left out of the checkpoint, and the next undo removed it.
//!
//! The snapshot now leaves what the binding leaves out of the working tree
//! out by name. A checkpoint names what it finds there, and an undo, which
//! would remove it, refuses.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::checkpoint::CheckpointRecord;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, TrackerMergeAction, Uncommitted, UndoOptions};
use newgit_core::store::MetadataStore;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "ana")
        .env("GIT_AUTHOR_EMAIL", "ana@example.com")
        .env("GIT_COMMITTER_NAME", "ana")
        .env("GIT_COMMITTER_EMAIL", "ana@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn read(workspace: &Utf8Path, path: &str) -> Option<String> {
    std::fs::read_to_string(workspace.join(path)).ok()
}

fn write(dir: &Utf8Path, path: &str, contents: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

/// What a checkpoint's snapshot changes from its HEAD, as `git diff-tree`
/// lists it: empty when it holds no uncommitted state.
fn snapshot(workspace: &Utf8Path, record: &CheckpointRecord) -> String {
    match &record.source.dirty_rev {
        Some(dirty) => git(
            workspace,
            &[
                "diff-tree",
                "-r",
                "--name-status",
                &record.source.head_rev,
                dirty,
            ],
        ),
        None => String::new(),
    }
}

/// Code trackers over the given paths, created in order, in a store whose
/// root owns `README.md`, with a resource `r` whose restore does nothing and
/// the other resources given.
fn setup(temp: &Utf8Path, trackers: &[(&str, &str)], resources: &[(&str, &str)]) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for (path, contents) in [
        ("README.md", "hello\n"),
        ("apps/b/index.js", "b v1\n"),
        ("apps/b/keep.js", "keep v1\n"),
        ("apps/b/inner/x.js", "inner v1\n"),
        ("apps/b/config.toml", "port = 1\n"),
    ] {
        write(&repo, path, contents);
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    std::fs::write(
        store.paths().resources.join("r.toml"),
        "ownership = \"branch\"\n\n[restore]\nmode = \"command\"\ncommand = \"true\"\n",
    )
    .expect("write resource");
    for (name, contents) in resources {
        std::fs::write(
            store.paths().resources.join(format!("{name}.toml")),
            contents,
        )
        .expect("write resource");
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);
    for (name, path) in trackers {
        BranchManager::open(store.clone())
            .expect("manager")
            .create_code_tracker(name, "project-devs", &[Utf8PathBuf::from(*path)])
            .expect("carve");
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    let manager = BranchManager::open(store).expect("manager");
    let published = manager
        .merge("main", "main")
        .expect("merge the project root");
    assert!(
        published
            .trackers
            .iter()
            .any(|merge| merge.tracker == "project" && merge.action == TrackerMergeAction::New),
        "{:?}",
        published.trackers
    );
    manager
}

/// Instance `x` with `b` dropped.
fn dropped_b(manager: &BranchManager) -> Utf8PathBuf {
    let workspace = manager
        .spawn("x", None)
        .expect("spawn")
        .branch
        .workspace_path;
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    assert!(!workspace.join("apps/b/index.js").exists());
    workspace
}

#[test]
fn a_checkpoint_leaves_out_and_names_what_a_dropped_trackers_paths_have() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, &[("b", "apps/b")], &[]);
    let workspace = dropped_b(&manager);

    // Written while `b` is dropped: a new file, and one of its tracked files
    // again, edited. Beside them, an edit to what the instance binds.
    write(&workspace, "apps/b/new.js", "new\n");
    write(&workspace, "apps/b/index.js", "b edited while dropped\n");
    write(&workspace, "README.md", "hello again\n");

    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    assert_eq!(snapshot(&workspace, &checkpoint.record), "M\tREADME.md");
    assert_eq!(
        checkpoint.warnings,
        vec![
            "`x` has changes in the paths of dropped tracker `b` (`apps/b/index.js`, \
             `apps/b/new.js`), and this checkpoint leaves them out: bind it again with \
             `newgit tracker use b` to keep them"
                .to_owned()
        ]
    );
    assert_eq!(read(&workspace, "apps/b/new.js").as_deref(), Some("new\n"));
    assert_eq!(
        read(&workspace, "apps/b/index.js").as_deref(),
        Some("b edited while dropped\n")
    );
}

#[test]
fn undo_refuses_rather_than_remove_an_edit_in_a_dropped_trackers_paths() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, &[("b", "apps/b")], &[]);
    let workspace = dropped_b(&manager);
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint").record.id;
    write(&workspace, "apps/b/index.js", "b edited while dropped\n");

    match manager.undo("x", &UndoOptions::default()) {
        Ok(undone) => panic!(
            "undo to {} went ahead, leaving apps/b/index.js {:?}; its checkpoint first, {}, \
             holds {:?}",
            undone.restored.id,
            read(&workspace, "apps/b/index.js"),
            undone.safety.id,
            snapshot(&workspace, &undone.safety)
        ),
        Err(refused) => assert_eq!(
            refused.to_string(),
            "`x` has changes in the paths of dropped tracker `b` (`apps/b/index.js`): a \
             checkpoint leaves them out, and undo would remove them; bind it again with \
             `newgit tracker use b` to keep them, or remove them"
        ),
    }
    assert_eq!(
        read(&workspace, "apps/b/index.js").as_deref(),
        Some("b edited while dropped\n")
    );
    // Refused before anything changed, the safety checkpoint included.
    let checkpoints: Vec<String> = manager
        .list_checkpoints("x")
        .expect("checkpoints")
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(checkpoints, vec![checkpoint]);
}

#[test]
fn undo_only_leaves_a_dropped_trackers_paths_as_they_are() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, &[("b", "apps/b")], &[]);
    let workspace = dropped_b(&manager);
    manager.checkpoint("x", None).expect("checkpoint");
    write(&workspace, "apps/b/new.js", "new\n");

    // `--only` leaves the source alone, so nothing there is at risk.
    let undone = manager
        .undo(
            "x",
            &UndoOptions {
                only: vec!["r".to_owned()],
                ..UndoOptions::default()
            },
        )
        .expect("undo --only");
    assert!(undone.partial);
    assert_eq!(
        undone.warnings,
        vec![
            "`x` has changes in the paths of dropped tracker `b` (`apps/b/new.js`), and this \
             checkpoint leaves them out: bind it again with `newgit tracker use b` to keep them"
                .to_owned()
        ]
    );
    assert_eq!(read(&workspace, "apps/b/new.js").as_deref(), Some("new\n"));
}

#[test]
fn a_checkpoint_keeps_a_bound_tracker_nested_in_a_dropped_one() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, &[("b", "apps/b"), ("inner", "apps/b/inner")], &[]);
    // `inner` stays bound, and the sparse checkout puts it back.
    let workspace = dropped_b(&manager);
    assert!(workspace.join("apps/b/inner/x.js").exists());

    write(&workspace, "apps/b/inner/x.js", "inner v2\n");
    write(&workspace, "apps/b/inner/y.js", "y\n");
    write(&workspace, "apps/b/new.js", "new\n");

    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    assert_eq!(
        snapshot(&workspace, &checkpoint.record),
        "M\tapps/b/inner/x.js\nA\tapps/b/inner/y.js"
    );
    assert_eq!(
        checkpoint.warnings,
        vec![
            "`x` has changes in the paths of dropped tracker `b` (`apps/b/new.js`), and this \
             checkpoint leaves them out: bind it again with `newgit tracker use b` to keep them"
                .to_owned()
        ]
    );
}

#[test]
fn a_checkpoint_keeps_the_deletion_of_a_bound_tracker_nested_in_a_dropped_one() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, &[("b", "apps/b"), ("inner", "apps/b/inner")], &[]);
    let workspace = dropped_b(&manager);

    // Nothing of `inner` left on disk: only HEAD still has its path.
    std::fs::remove_dir_all(workspace.join("apps/b/inner")).expect("rm");

    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    assert_eq!(
        snapshot(&workspace, &checkpoint.record),
        "D\tapps/b/inner/x.js"
    );
    assert_eq!(checkpoint.warnings, Vec::<String>::new());
}

#[test]
fn binding_the_tracker_again_lets_undo_keep_what_its_paths_have() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, &[("b", "apps/b")], &[]);
    let workspace = dropped_b(&manager);
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint").record.id;
    write(&workspace, "apps/b/index.js", "b edited while dropped\n");
    write(&workspace, "apps/b/new.js", "new\n");
    manager
        .undo("x", &UndoOptions::default())
        .expect_err("refused while they are there");

    // What the refusal says to do: `b`'s paths come back into the
    // instance's scope, and the checkpoint undo takes first holds them.
    manager.use_tracker("x", "b").expect("use b");
    let undone = manager.undo("x", &UndoOptions::default()).expect("undo");
    assert_eq!(undone.restored.id, checkpoint);
    assert_eq!(read(&workspace, "apps/b/new.js"), None);

    manager
        .undo(
            "x",
            &UndoOptions {
                to: Some(undone.safety.id.clone()),
                ..UndoOptions::default()
            },
        )
        .expect("redo");
    assert_eq!(
        read(&workspace, "apps/b/index.js").as_deref(),
        Some("b edited while dropped\n")
    );
    assert_eq!(read(&workspace, "apps/b/new.js").as_deref(), Some("new\n"));
}

/// A resource rendering its port into `apps/b/config.toml`, which `b` owns.
const RENDERS_INTO_B: &str = r#"ownership = "branch"

[ports]
api = { start = 47520 }

[[render]]
path = "apps/b/config.toml"
replace = [
  { find = "port = 1", with = "port = {{ports.api}}" },
]
"#;

#[test]
fn a_rendered_file_in_a_dropped_trackers_paths_is_not_named() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, &[("b", "apps/b")], &[("svc", RENDERS_INTO_B)]);
    let workspace = dropped_b(&manager);
    // The render stays on disk and skip-worktree, out of `git status`. What
    // a checkpoint names is read past that bit, as a dropped tracker's file
    // written again needs, and that sees this instance's port as a change.
    assert_eq!(
        git(&workspace, &["ls-files", "-t", "--", "apps/b/config.toml"]),
        "S apps/b/config.toml"
    );
    assert_eq!(
        git(&workspace, &["status", "--porcelain", "--", "apps/b"]),
        ""
    );

    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    assert_eq!(checkpoint.warnings, Vec::<String>::new());
    manager
        .undo("x", &UndoOptions::default())
        .expect("undo, which renders the file again");
}
