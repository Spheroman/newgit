//! Undo reapplies a checkpoint's uncommitted state by checking the
//! snapshot's tree out over the workspace. Its removal reaches only entries
//! that are not skip-worktree, but it wrote each path where the tree
//! differs from the index, outside the sparse patterns too, and cleared its
//! skip-worktree bit, so a dropped code tracker's files came back on disk as
//! ordinary tracked files (#225). The restore now checks out a tree with
//! the paths the binding leaves out as HEAD has them, so they stay out.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BranchManager, TrackerMergeAction, Uncommitted, UndoOptions};
use newgit_core::source::GitSource;
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

fn status(workspace: &Utf8Path) -> String {
    git(
        workspace,
        &["status", "--porcelain", "--untracked-files=all"],
    )
}

fn read(workspace: &Utf8Path, path: &str) -> Option<String> {
    std::fs::read_to_string(workspace.join(path)).ok()
}

/// A repository with `a/f`, `b/f` and `b/inner/x` committed, reset to that
/// commit under `sparse` patterns, and a snapshot of uncommitted state on
/// top of it: `a/f`, `b/f` and `b/inner/x` edited, `b/new` and `b/inner/y`
/// added. Returns the repository, the commit and the snapshot.
fn snapshot(temp: &Utf8Path, sparse: &[&str]) -> (Utf8PathBuf, String, String) {
    let ws = temp.join("ws");
    std::fs::create_dir_all(ws.join("a")).expect("mkdir");
    std::fs::create_dir_all(ws.join("b/inner")).expect("mkdir");
    git(&ws, &["init", "-q", "-b", "main"]);
    for (path, contents) in [("a/f", "a\n"), ("b/f", "b\n"), ("b/inner/x", "x\n")] {
        std::fs::write(ws.join(path), contents).expect("write");
    }
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "initial"]);
    let head = git(&ws, &["rev-parse", "HEAD"]);
    for (path, contents) in [
        ("a/f", "a2\n"),
        ("b/f", "b2\n"),
        ("b/new", "new\n"),
        ("b/inner/x", "x2\n"),
        ("b/inner/y", "y\n"),
    ] {
        std::fs::write(ws.join(path), contents).expect("write");
    }
    git(&ws, &["add", "-A"]);
    let tree = git(&ws, &["write-tree"]);
    let dirty = git(&ws, &["commit-tree", &tree, "-p", &head, "-m", "snapshot"]);
    git(&ws, &["reset", "-q", "--hard", &head]);
    git(&ws, &["clean", "-fdq"]);
    let mut set = vec!["sparse-checkout", "set", "--no-cone"];
    set.extend(sparse);
    git(&ws, &set);
    (ws, head, dirty)
}

#[test]
fn a_restore_leaves_a_dropped_trackers_paths_out_of_the_working_tree() {
    let (_guard, temp) = tempdir();
    // `b` left out, as `apply_sparse` writes it for a dropped tracker.
    let (ws, head, dirty) = snapshot(&temp, &["/*", "!/b/", "!/b"]);
    assert!(
        !ws.join("b").exists(),
        "precondition: `b` is out of the tree"
    );

    GitSource::workspace_restore_to(&ws, &head, Some(&dirty), &[Utf8Path::new("b")], &[])
        .expect("restore");

    assert!(
        !ws.join("b").exists(),
        "the restore wrote into `b`, which the sparse checkout leaves out: b/f {:?}, b/new {:?}",
        read(&ws, "b/f"),
        read(&ws, "b/new")
    );
    assert_eq!(
        git(&ws, &["ls-files", "-t", "--", "b"]),
        "S b/f\nS b/inner/x",
        "`b`'s entries stay skip-worktree, and `b/new` is in no index"
    );
    assert_eq!(read(&ws, "a/f").as_deref(), Some("a2\n"));
    assert_eq!(status(&ws), " M a/f");
}

#[test]
fn a_restore_reapplies_a_bound_tracker_nested_in_a_dropped_one() {
    let (_guard, temp) = tempdir();
    // `b` left out, and `b/inner` — a bound tracker nested in it — put back.
    let (ws, head, dirty) = snapshot(&temp, &["/*", "!/b/", "!/b", "/b/inner/", "/b/inner"]);

    GitSource::workspace_restore_to(
        &ws,
        &head,
        Some(&dirty),
        &[Utf8Path::new("b")],
        &[Utf8Path::new("b/inner")],
    )
    .expect("restore");

    assert_eq!(read(&ws, "b/inner/x").as_deref(), Some("x2\n"));
    assert_eq!(read(&ws, "b/inner/y").as_deref(), Some("y\n"));
    assert_eq!(read(&ws, "b/f"), None);
    assert_eq!(read(&ws, "b/new"), None);
    assert_eq!(
        git(&ws, &["ls-files", "-t", "--", "b"]),
        "S b/f\nH b/inner/x"
    );
    assert_eq!(status(&ws), " M a/f\n M b/inner/x\n?? b/inner/y");
}

/// Code trackers over the given paths, created in order, in a store whose
/// root owns `lib/` and everything else.
fn setup(temp: &Utf8Path, trackers: &[(&str, &str)]) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("apps/b/inner")).expect("mkdir");
    std::fs::create_dir_all(repo.join("lib")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for (path, contents) in [
        ("README.md", "hello\n"),
        ("apps/b/index.js", "b v1\n"),
        ("apps/b/keep.js", "keep v1\n"),
        ("apps/b/inner/x.js", "inner v1\n"),
        ("lib/util.js", "util v1\n"),
    ] {
        std::fs::write(repo.join(path), contents).expect("write");
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
    let published = BranchManager::open(store.clone())
        .expect("manager")
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
    store
}

#[test]
fn undo_leaves_out_what_a_dropped_tracker_owns_though_the_checkpoint_has_it_changed() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, &[("b", "apps/b")]);
    let manager = BranchManager::open(store.clone()).expect("manager");
    let workspace = manager
        .spawn("x", None)
        .expect("spawn")
        .branch
        .workspace_path;

    // Checkpointed with `b` dropped and an edit in `lib`, root's.
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    std::fs::write(workspace.join("lib/util.js"), "util v2\n").expect("write");
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint").record.id;
    manager.use_tracker("x", "b").expect("use b");

    // `b` takes `lib` over. Only an edited definition does that — a code
    // tracker's paths are set when it is created — but the definitions are
    // the project's committed files, and a pull brings a teammate's edit.
    let definition = store.paths().trackers.join("b.toml");
    let before = std::fs::read_to_string(&definition).expect("read");
    let after = before.replace(r#"paths = ["apps/b"]"#, r#"paths = ["apps/b", "lib"]"#);
    assert_ne!(before, after, "precondition: {before}");
    std::fs::write(&definition, after).expect("write");
    git(&temp.join("store"), &["commit", "-q", "-am", "b owns lib"]);
    let manager = BranchManager::open(store).expect("manager");

    let undone = manager
        .undo(
            "x",
            &UndoOptions {
                to: Some(checkpoint.clone()),
                ..UndoOptions::default()
            },
        )
        .expect("undo");
    assert_eq!(undone.restored.id, checkpoint);
    let branch = manager.store().find_branch("x").expect("instance");
    assert!(branch.dropped.contains("b"), "{:?}", branch.dropped);

    assert!(
        !workspace.join("lib").exists(),
        "undo wrote the checkpoint's `lib/util.js` ({:?}) into `b`, which it left dropped",
        read(&workspace, "lib/util.js")
    );
    assert_eq!(
        git(&workspace, &["ls-files", "-t", "--", "lib", "apps/b"]),
        "S apps/b/index.js\nS apps/b/inner/x.js\nS apps/b/keep.js\nS lib/util.js"
    );
    assert_eq!(status(&workspace), "");
}

#[test]
fn undo_reapplies_a_bound_tracker_nested_in_a_dropped_one() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, &[("b", "apps/b"), ("inner", "apps/b/inner")]);
    let manager = BranchManager::open(store).expect("manager");
    let workspace = manager
        .spawn("x", None)
        .expect("spawn")
        .branch
        .workspace_path;

    // `b` dropped, `inner` still bound: the sparse checkout puts it back.
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    std::fs::write(workspace.join("apps/b/inner/x.js"), "inner v2\n").expect("write");
    std::fs::write(workspace.join("apps/b/inner/y.js"), "y\n").expect("write");
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint").record.id;
    std::fs::write(workspace.join("apps/b/inner/x.js"), "inner v3\n").expect("write");
    std::fs::remove_file(workspace.join("apps/b/inner/y.js")).expect("rm");

    let undone = manager.undo("x", &UndoOptions::default()).expect("undo");
    assert_eq!(undone.restored.id, checkpoint);

    assert_eq!(
        read(&workspace, "apps/b/inner/x.js").as_deref(),
        Some("inner v2\n")
    );
    assert_eq!(
        read(&workspace, "apps/b/inner/y.js").as_deref(),
        Some("y\n")
    );
    assert_eq!(read(&workspace, "apps/b/index.js"), None);
    assert_eq!(
        git(&workspace, &["ls-files", "-t", "--", "apps/b"]),
        "S apps/b/index.js\nH apps/b/inner/x.js\nS apps/b/keep.js"
    );
    assert_eq!(
        status(&workspace),
        " M apps/b/inner/x.js\n?? apps/b/inner/y.js"
    );
}
