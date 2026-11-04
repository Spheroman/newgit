//! A rendered file is marked `--skip-worktree`, so this instance's values
//! never show in `git status` and `git add -A` cannot commit them. Dropping
//! a code tracker that owns paths turns the workspace into a sparse
//! checkout, and a sparse checkout decides that bit itself:
//! `sparse-checkout set` and `disable` work every file's bit out again from
//! the patterns, Git commands that update the working tree do too, and
//! reading the index clears the bit of every marked file Git finds on disk.
//! So once a code tracker was dropped, the rendered file showed as
//! modified, an agent's `git add -A && git commit` committed its port, and
//! a checkpoint held it.
//!
//! The patterns now leave the rendered files out too, and the workspace
//! sets `sparse.expectFilesOutsideOfPatterns`. That setting also keeps the
//! bit on a dropped tracker's file written again on disk, so `git status`
//! and `tracker drop` leave that file alone, while a checkpoint and undo
//! still see it; the last tests pin what each now does with it.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{
    BindingChoice, BranchManager, TrackerMergeAction, Uncommitted, UndoOptions,
};
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

/// `git ls-files -t`'s tag for `path`: `S` when it is skip-worktree.
fn tag(workspace: &Utf8Path, path: &str) -> String {
    git(workspace, &["ls-files", "-t", "--", path])
}

fn status(workspace: &Utf8Path) -> String {
    git(
        workspace,
        &["status", "--porcelain", "--untracked-files=all"],
    )
}

/// A resource, its port and the first number tried for it, the file it
/// renders, and the committed text its port replaces there.
type Render = (&'static str, &'static str, u16, &'static str, &'static str);

/// Resource `svc` renders its port into the root-owned `config.toml`, and
/// `web` into `apps/a/config.toml`, inside tracker `a`'s paths.
const RENDERS: &[Render] = &[
    ("svc", "api", 47300, "config.toml", "port = 1"),
    ("web", "web", 47400, "apps/a/config.toml", "port = 2"),
];

/// `RENDERS`, with code trackers over the given paths created in order.
fn setup(temp: &Utf8Path, trackers: &[(&str, &str)]) -> BranchManager {
    setup_with(temp, trackers, RENDERS)
}

fn setup_with(temp: &Utf8Path, trackers: &[(&str, &str)], renders: &[Render]) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for (path, contents) in [
        ("README.md", "hello\n"),
        ("config.toml", "port = 1\n"),
        ("apps/a/index.js", "a v1\n"),
        ("apps/a/config.toml", "port = 2\n"),
        ("apps/b/index.js", "b v1\n"),
        ("apps/b/keep.js", "keep v1\n"),
        ("apps/b/inner/x.js", "inner v1\n"),
        ("apps/b/conf/config.toml", "port = 3\n"),
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
    for (name, port, start, path, find) in renders {
        std::fs::write(
            store.paths().resources.join(format!("{name}.toml")),
            format!(
                "ownership = \"branch\"\n\n[ports]\n{port} = {{ start = {start} }}\n\n\
                 [[render]]\npath = \"{path}\"\nreplace = [\n  {{ find = \"{find}\", with = \
                 \"port = {{{{ports.{port}}}}}\" }},\n]\n"
            ),
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

const AB: &[(&str, &str)] = &[("a", "apps/a"), ("b", "apps/b")];

/// What `svc` rendered into `config.toml` in `instance`.
fn rendered(manager: &BranchManager, instance: &str) -> String {
    let branch = manager.store().find_branch(instance).expect("instance");
    format!("port = {}\n", branch.resources["svc"].resolved_ports["api"])
}

/// Instance `x`, with `b` dropped.
fn dropped_b(manager: &BranchManager) -> Utf8PathBuf {
    let workspace = manager
        .spawn("x", None)
        .expect("spawn")
        .branch
        .workspace_path;
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    assert!(!workspace.join("apps/b/index.js").exists());
    workspace
}

#[test]
fn a_rendered_file_stays_out_of_git_status_and_git_add_once_a_code_tracker_is_dropped() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);

    assert_eq!(
        read(&workspace, "config.toml"),
        Some(rendered(&manager, "x"))
    );
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
    assert_eq!(status(&workspace), "");

    // An agent committing everything commits none of this instance's values.
    git(&workspace, &["add", "-A"]);
    assert_eq!(git(&workspace, &["diff", "--cached", "--name-only"]), "");
}

#[test]
fn a_checkpoint_taken_while_a_code_tracker_is_dropped_holds_no_rendered_value() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);
    write(&workspace, "README.md", "hello agent\n");

    let outcome = manager.checkpoint("x", None).expect("checkpoint");
    let dirty = outcome.record.source.dirty_rev.expect("README.md is dirty");
    assert_eq!(
        git(
            &workspace,
            &["diff-tree", "-r", "--name-status", "HEAD", &dirty]
        ),
        "M\tREADME.md"
    );
}

/// A workspace whose tracker an earlier newgit dropped has neither the
/// rendered files in its patterns nor the setting (`false` is its default).
/// The snapshot sets it for itself, so the marks it puts on the rendered
/// files hold there.
#[test]
fn a_checkpoint_holds_no_rendered_value_in_a_sparse_workspace_without_the_setting() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);
    git(
        &workspace,
        &[
            "sparse-checkout",
            "set",
            "--no-cone",
            "/*",
            "!/apps/b/",
            "!/apps/b",
        ],
    );
    git(
        &workspace,
        &["config", "sparse.expectFilesOutsideOfPatterns", "false"],
    );
    assert!(
        status(&workspace).contains(" M config.toml"),
        "{}",
        status(&workspace)
    );
    write(&workspace, "README.md", "hello agent\n");

    let outcome = manager.checkpoint("x", None).expect("checkpoint");
    let dirty = outcome.record.source.dirty_rev.expect("README.md is dirty");
    assert_eq!(
        git(
            &workspace,
            &["diff-tree", "-r", "--name-status", "HEAD", &dirty]
        ),
        "M\tREADME.md"
    );
}

/// A rendered file in a bound tracker nested in a dropped one, here all that
/// tracker has: the snapshot adds such a tracker's paths on their own, and
/// the mark holds there too, with the setting and, set back to the patterns
/// an earlier newgit wrote, without it. Undo reapplies those paths on their
/// own as well, and leaves the mark on.
#[test]
fn a_checkpoint_holds_no_rendered_value_in_a_bound_tracker_nested_in_a_dropped_one() {
    let (_guard, temp) = tempdir();
    let mut renders = RENDERS.to_vec();
    renders.push(("deep", "deep", 47500, "apps/b/conf/config.toml", "port = 3"));
    let manager = setup_with(&temp, &[("b", "apps/b"), ("conf", "apps/b/conf")], &renders);
    let workspace = dropped_b(&manager);
    assert_eq!(
        tag(&workspace, "apps/b/conf/config.toml"),
        "S apps/b/conf/config.toml"
    );
    assert_eq!(status(&workspace), "");
    write(&workspace, "README.md", "hello agent\n");
    let snapshot = || {
        let outcome = manager.checkpoint("x", None).expect("checkpoint");
        let dirty = outcome.record.source.dirty_rev.expect("README.md is dirty");
        git(
            &workspace,
            &["diff-tree", "-r", "--name-status", "HEAD", &dirty],
        )
    };
    assert_eq!(snapshot(), "M\tREADME.md");
    manager.undo("x", &UndoOptions::default()).expect("undo");
    assert_eq!(
        tag(&workspace, "apps/b/conf/config.toml"),
        "S apps/b/conf/config.toml"
    );
    assert_eq!(status(&workspace), " M README.md");

    git(
        &workspace,
        &[
            "sparse-checkout",
            "set",
            "--no-cone",
            "/*",
            "!/apps/b/",
            "!/apps/b",
            "/apps/b/conf/",
            "/apps/b/conf",
        ],
    );
    git(
        &workspace,
        &["config", "sparse.expectFilesOutsideOfPatterns", "false"],
    );
    assert!(
        status(&workspace).contains(" M apps/b/conf/config.toml"),
        "{}",
        status(&workspace)
    );
    assert_eq!(snapshot(), "M\tREADME.md");
}

#[test]
fn a_rendered_file_keeps_its_mark_once_the_dropped_tracker_is_bound_again() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);

    manager.use_tracker("x", "b").expect("use b");
    assert_eq!(
        read(&workspace, "apps/b/index.js").as_deref(),
        Some("b v1\n")
    );
    assert_eq!(
        read(&workspace, "config.toml"),
        Some(rendered(&manager, "x"))
    );
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
    assert_eq!(status(&workspace), "");
}

#[test]
fn a_rendered_file_keeps_its_mark_in_an_instance_spawned_with_a_tracker_dropped() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let choice = BindingChoice {
        drops: vec!["b".to_owned()],
        ..BindingChoice::default()
    };
    let workspace = manager
        .spawn_binding("x", None, &choice)
        .expect("spawn --drop b")
        .branch
        .workspace_path;

    assert!(!workspace.join("apps/b/index.js").exists());
    assert_eq!(
        read(&workspace, "config.toml"),
        Some(rendered(&manager, "x"))
    );
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
    assert_eq!(status(&workspace), "");

    // The render came after the spawn's sparse checkout, and the patterns
    // took it in afterwards: `reset --hard` works the bit out from them.
    git(&workspace, &["reset", "-q", "--hard"]);
    assert_eq!(
        read(&workspace, "config.toml"),
        Some(rendered(&manager, "x"))
    );
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
}

/// A workspace whose trackers an earlier newgit dropped: no rendered file in
/// its patterns, no setting, and the marks gone. The next `tracker use` or
/// `drop` puts them back, before the patterns that leave the rendered files
/// out are set: unmarked, a file holding what the index does would be taken
/// off disk.
#[test]
fn a_workspace_an_earlier_newgit_made_sparse_gets_its_marks_back() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);
    manager
        .drop_tracker("x", "a", Uncommitted::Refuse)
        .expect("drop a");
    let as_before = |workspace: &Utf8Path, dropped: &[&str]| {
        let mut args = vec!["sparse-checkout", "set", "--no-cone", "/*"];
        for path in dropped {
            args.extend([*path, &path[..path.len() - 1]]);
        }
        git(workspace, &args);
        git(
            workspace,
            &["config", "sparse.expectFilesOutsideOfPatterns", "false"],
        );
    };
    // `web`'s file stayed on disk when `a` was dropped, and shows too.
    as_before(&workspace, &["!/apps/a/", "!/apps/b/"]);
    assert_eq!(status(&workspace), " M apps/a/config.toml\n M config.toml");

    manager.use_tracker("x", "b").expect("use b");
    assert_eq!(
        read(&workspace, "config.toml"),
        Some(rendered(&manager, "x"))
    );
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
    assert_eq!(status(&workspace), "");

    // There, an agent's `reset --hard` put the committed value back.
    as_before(&workspace, &["!/apps/a/"]);
    git(&workspace, &["reset", "-q", "--hard"]);
    assert_eq!(
        read(&workspace, "config.toml").as_deref(),
        Some("port = 1\n")
    );
    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    assert_eq!(
        read(&workspace, "config.toml").as_deref(),
        Some("port = 1\n")
    );
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
}

/// Each of these works out every file's skip-worktree bit again from the
/// sparse patterns, which took the rendered file in, so it came out
/// unmarked; `reset --hard` also put the committed `port = 1` back on disk.
#[test]
fn git_commands_that_update_the_working_tree_keep_a_rendered_file_while_a_tracker_is_dropped() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);
    let expected = rendered(&manager, "x");

    write(&workspace, "README.md", "hello agent\n");
    git(&workspace, &["stash", "-q"]);
    git(&workspace, &["stash", "pop", "-q"]);
    assert_eq!(
        read(&workspace, "config.toml"),
        Some(expected.clone()),
        "stash"
    );
    assert_eq!(status(&workspace), " M README.md", "stash");

    git(&workspace, &["reset", "-q", "--hard"]);
    assert_eq!(
        read(&workspace, "config.toml"),
        Some(expected.clone()),
        "reset"
    );
    assert_eq!(status(&workspace), "", "reset --hard");

    git(&workspace, &["checkout", "-q", "-b", "feature", "HEAD~1"]);
    assert_eq!(read(&workspace, "config.toml"), Some(expected), "checkout");
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml", "checkout");
    assert_eq!(status(&workspace), "", "checkout");
}

#[test]
fn undo_while_a_code_tracker_is_dropped_leaves_the_rendered_file_marked() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);
    manager.checkpoint("x", None).expect("checkpoint");
    write(&workspace, "README.md", "hello agent\n");

    let undone = manager.undo("x", &UndoOptions::default()).expect("undo");
    assert_eq!(undone.restored.source.dirty_rev, None, "{undone:?}");
    assert_eq!(read(&workspace, "README.md").as_deref(), Some("hello\n"));
    assert_eq!(
        read(&workspace, "config.toml"),
        Some(rendered(&manager, "x"))
    );
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
    assert_eq!(status(&workspace), "");
}

/// The record of what was rendered can name a file the index no longer
/// holds, and Git marks none of a list that names one. The drop marks the
/// rest.
#[test]
fn tracker_drop_goes_ahead_once_a_rendered_file_has_left_the_index() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = manager
        .spawn("x", None)
        .expect("spawn")
        .branch
        .workspace_path;
    git(&workspace, &["rm", "-q", "--sparse", "config.toml"]);
    git(&workspace, &["commit", "-q", "-m", "no config.toml"]);

    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    assert_eq!(
        tag(&workspace, "apps/a/config.toml"),
        "S apps/a/config.toml"
    );
    assert_eq!(status(&workspace), "");
    manager.use_tracker("x", "b").expect("use b");
    assert_eq!(
        tag(&workspace, "apps/a/config.toml"),
        "S apps/a/config.toml"
    );
    assert_eq!(status(&workspace), "");
}

/// A rendered file inside a code tracker's paths is no uncommitted work of
/// that tracker's. While another tracker was dropped, its cleared bit made
/// `tracker drop a` refuse over it.
#[test]
fn tracker_drop_is_not_refused_over_a_rendered_file_in_its_paths() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);

    manager
        .drop_tracker("x", "a", Uncommitted::Refuse)
        .expect("drop a");
    assert!(!workspace.join("apps/a/index.js").exists());
    assert_eq!(status(&workspace), "");
}

/// `sparse.expectFilesOutsideOfPatterns` keeps the bit on a dropped
/// tracker's file that is written again, as it keeps it on a rendered file,
/// so `git status` does not show the edit. A checkpoint still names it, and
/// undo, which would remove it, still refuses: they read the working tree
/// past that bit, and leave every file's bit as it was. Binding the tracker
/// again brings the edit into view.
#[test]
fn a_checkpoint_and_undo_still_see_a_file_written_again_in_a_dropped_trackers_paths() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp, AB);
    let workspace = dropped_b(&manager);
    manager.checkpoint("x", None).expect("checkpoint");
    write(&workspace, "apps/b/index.js", "b edited while dropped\n");
    assert_eq!(status(&workspace), "");

    let named = "`x` has changes in the paths of dropped tracker `b` (`apps/b/index.js`)";
    let checkpoint = manager.checkpoint("x", None).expect("checkpoint");
    assert!(
        checkpoint
            .warnings
            .iter()
            .any(|warning| warning.starts_with(named)),
        "{:?}",
        checkpoint.warnings
    );
    let refused = manager
        .undo("x", &UndoOptions::default())
        .expect_err("undo would remove the edit");
    assert!(refused.to_string().starts_with(named), "{refused}");
    assert_eq!(
        read(&workspace, "apps/b/index.js").as_deref(),
        Some("b edited while dropped\n")
    );
    assert_eq!(tag(&workspace, "apps/b/index.js"), "S apps/b/index.js");
    assert_eq!(tag(&workspace, "config.toml"), "S config.toml");
    assert_eq!(status(&workspace), "");

    manager.use_tracker("x", "b").expect("use b");
    assert_eq!(
        read(&workspace, "apps/b/index.js").as_deref(),
        Some("b edited while dropped\n")
    );
    assert_eq!(status(&workspace), " M apps/b/index.js");
}

/// `tracker drop`'s check for uncommitted work goes by `git status`, so a
/// file of a dropped tracker nested in the one being dropped, written
/// again, is not that check's to see, and the drop leaves it on disk.
#[test]
fn tracker_drop_leaves_a_file_written_again_in_a_nested_dropped_trackers_paths() {
    let (_guard, temp) = tempdir();
    let manager = setup(
        &temp,
        &[("a", "apps/a"), ("b", "apps/b"), ("inner", "apps/b/inner")],
    );
    let workspace = manager
        .spawn("x", None)
        .expect("spawn")
        .branch
        .workspace_path;
    manager
        .drop_tracker("x", "inner", Uncommitted::Refuse)
        .expect("drop inner");
    write(
        &workspace,
        "apps/b/inner/x.js",
        "inner edited while dropped\n",
    );

    manager
        .drop_tracker("x", "b", Uncommitted::Refuse)
        .expect("drop b");
    assert!(!workspace.join("apps/b/index.js").exists());
    assert_eq!(
        read(&workspace, "apps/b/inner/x.js").as_deref(),
        Some("inner edited while dropped\n")
    );

    manager.use_tracker("x", "b").expect("use b");
    manager.use_tracker("x", "inner").expect("use inner");
    assert_eq!(status(&workspace), " M apps/b/inner/x.js");
}
