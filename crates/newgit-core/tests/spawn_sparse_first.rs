//! A spawn that leaves a tracker out never writes its files. It used to check
//! everything out and then delete what the binding left out, so leaving 29,000
//! files out made a spawn slower than bringing them, not faster.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

mod imports;

use camino::{Utf8Path, Utf8PathBuf};
use imports::*;
use newgit_core::SourceSubstrate;
use newgit_core::cleanup::ArchivedCheckpoints;
use newgit_core::manager::{BindingChoice, SourceBranchPolicy};
use newgit_core::source::GitSource;
use newgit_core::store::MetadataStore;

const LONGER_THAN_ANY_FILE_NAME: usize = 300;

/// A project with tracker `b` at `apps/b`, published. `apps/b` holds a file
/// with a name no file system can hold, so anything that tries to write it
/// fails: a test of what a spawn does not do, needing no clock.
fn with_unwritable_tracker(temp: &Utf8Path) -> MetadataStore {
    let store = project(temp);
    let repo = store.paths().project_root.clone();
    commit_as(
        &repo,
        "ana",
        1_700_000_000,
        &[("apps/b/index.js", "b\n"), ("keep.js", "keep\n")],
        "b",
    );
    manager(&store)
        .create_git_tracker("b", "project-devs", &[Utf8PathBuf::from("apps/b")])
        .expect("carve");
    manager(&store).merge("main", "main").expect("publish");
    let blob = git_input(&repo, &["hash-object", "-w", "--stdin"], "x\n");
    let name = "n".repeat(LONGER_THAN_ANY_FILE_NAME);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},apps/b/{name}"),
        ],
    );
    git(
        &repo,
        &["commit", "-q", "-m", "A name no file system holds"],
    );
    manager(&store).merge("main", "main").expect("publish");
    store
}

fn git_input(dir: &Utf8Path, args: &[&str], input: &str) -> String {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(input.as_bytes())
        .expect("write");
    let output = child.wait_with_output().expect("git finishes");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn without_b() -> BindingChoice {
    BindingChoice {
        drops: vec!["b".to_owned()],
        ..BindingChoice::default()
    }
}

#[test]
fn a_spawn_never_writes_what_it_leaves_out() {
    let (_guard, temp) = tempdir();
    let store = with_unwritable_tracker(&temp);
    let spawned = manager(&store)
        .spawn_binding("x", None, &without_b())
        .expect("nothing tried to write the file");
    assert_eq!(spawned.template_warning, None, "no fallback either");
    let workspace = spawned.branch.workspace_path;
    assert!(workspace.join("keep.js").is_file());
    assert!(!workspace.join("apps/b").exists());
    assert_eq!(git(&workspace, &["status", "--porcelain"]), "");
    let marks = git(&workspace, &["ls-files", "-t", "--", "apps/b/index.js"]);
    assert_eq!(marks, "S apps/b/index.js");
    assert_eq!(
        git(&workspace, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/x"
    );
}

/// Without a template, as off macOS and when one fails.
#[test]
fn a_plain_clone_never_writes_what_it_leaves_out_either() {
    let (_guard, temp) = tempdir();
    let store = with_unwritable_tracker(&temp);
    let repo = store.paths().project_root.clone();
    git(&repo, &["branch", "plain", "main"]);
    let source = GitSource::open(&repo, SourceSubstrate::Git).expect("source");
    let destination = temp.join("plain");
    let sparse = ["/*", "!/apps/b/", "!/apps/b"].map(ToOwned::to_owned);
    source
        .clone_to("plain", &destination, &sparse)
        .expect("nothing tried to write the file");
    assert!(destination.join("keep.js").is_file());
    assert!(!destination.join("apps/b").exists());
    assert_eq!(git(&destination, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&destination, &["symbolic-ref", "HEAD"]),
        "refs/heads/plain"
    );
}

#[test]
fn a_failed_plain_clone_leaves_nothing_behind() {
    let (_guard, temp) = tempdir();
    let store = with_unwritable_tracker(&temp);
    let repo = store.paths().project_root.clone();
    git(&repo, &["branch", "plain", "main"]);
    let source = GitSource::open(&repo, SourceSubstrate::Git).expect("source");
    let destination = temp.join("plain");
    // Nothing left out, so the unwritable file is written, and fails.
    let sparse = ["/*".to_owned()];
    source
        .clone_to("plain", &destination, &sparse)
        .expect_err("cannot write it");
    assert!(!destination.exists());
}

fn sparse_templates(temp: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut found: Vec<Utf8PathBuf> = std::fs::read_dir(temp.join("workspaces"))
        .expect("workspaces")
        .map(|entry| Utf8PathBuf::from_path_buf(entry.expect("entry").path()).expect("utf8"))
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.starts_with(".source-template-"))
        })
        .collect();
    found.sort();
    found
}

/// `b` and a file beside it, with nothing unwritable, for the template tests.
fn plain_tracker(temp: &Utf8Path) -> MetadataStore {
    let store = project(temp);
    let repo = store.paths().project_root.clone();
    commit_as(
        &repo,
        "ana",
        1_700_000_000,
        &[("apps/b/index.js", "b\n"), ("keep.js", "keep\n")],
        "b",
    );
    manager(&store)
        .create_git_tracker("b", "project-devs", &[Utf8PathBuf::from("apps/b")])
        .expect("carve");
    manager(&store).merge("main", "main").expect("publish");
    store
}

#[cfg(target_os = "macos")]
#[test]
fn each_binding_has_a_template_that_never_held_what_it_leaves_out() {
    let (_guard, temp) = tempdir();
    let store = plain_tracker(&temp);
    manager(&store)
        .spawn_binding("x", None, &without_b())
        .expect("spawn");
    assert!(
        !temp.join("workspaces/.source-template").exists(),
        "no full template was needed"
    );
    let templates = sparse_templates(&temp);
    assert_eq!(templates.len(), 1, "{templates:?}");
    assert!(!templates[0].join("apps/b").exists());
    assert!(templates[0].join("keep.js").is_file());

    manager(&store)
        .spawn_binding("y", None, &without_b())
        .expect("spawn");
    assert_eq!(
        sparse_templates(&temp),
        templates,
        "the same binding shares it"
    );

    let bound = manager(&store).spawn("z", None).expect("spawn");
    assert!(
        bound
            .branch
            .workspace_path
            .join("apps/b/index.js")
            .is_file()
    );
    assert_eq!(
        git(&bound.branch.workspace_path, &["status", "--porcelain"]),
        ""
    );
    assert_eq!(sparse_templates(&temp), templates);
}

/// A second spawn takes what `main` has now, and the instance can still bind
/// what it left out.
#[test]
fn a_later_spawn_has_what_moved_and_can_bring_back_what_it_left_out() {
    let (_guard, temp) = tempdir();
    let store = plain_tracker(&temp);
    let repo = store.paths().project_root.clone();
    manager(&store)
        .spawn_binding("first", None, &without_b())
        .expect("spawn");
    commit_as(
        &repo,
        "ana",
        1_700_000_100,
        &[("later.js", "later\n"), ("apps/b/later.js", "b later\n")],
        "later",
    );
    let workspace = manager(&store)
        .spawn_binding("second", None, &without_b())
        .expect("spawn")
        .branch
        .workspace_path;
    assert!(workspace.join("later.js").is_file());
    assert!(!workspace.join("apps/b").exists());
    assert_eq!(git(&workspace, &["status", "--porcelain"]), "");

    manager(&store).use_tracker("second", "b").expect("use");
    assert!(workspace.join("apps/b/index.js").is_file());
    assert!(workspace.join("apps/b/later.js").is_file());
    assert_eq!(git(&workspace, &["status", "--porcelain"]), "");
}

#[cfg(target_os = "macos")]
#[test]
fn a_failing_sparse_template_falls_back_and_still_writes_nothing_it_leaves_out() {
    let (_guard, temp) = tempdir();
    let store = with_unwritable_tracker(&temp);
    manager(&store)
        .spawn_binding("x", None, &without_b())
        .expect("spawn");
    let template = sparse_templates(&temp).remove(0);
    std::fs::remove_dir_all(&template).expect("remove");
    std::fs::write(&template, "").expect("a file where it belongs");

    let spawned = manager(&store)
        .spawn_binding("y", None, &without_b())
        .expect("the fallback clones instead");
    assert!(spawned.template_warning.is_some(), "and says so");
    assert!(!spawned.branch.workspace_path.join("apps/b").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn cleanup_keeps_a_template_in_use_and_removes_one_that_is_not() {
    let (_guard, temp) = tempdir();
    let store = plain_tracker(&temp);
    let repo = store.paths().project_root.clone();
    manager(&store)
        .spawn_binding("x", None, &without_b())
        .expect("spawn");
    let template = sparse_templates(&temp).remove(0);

    let kept = manager(&store)
        .cleanup(false, ArchivedCheckpoints::Keep)
        .expect("cleanup");
    assert!(template.is_dir(), "a live binding uses it");
    assert!(!kept.orphan_workspaces.contains(&template));
    assert!(
        !kept
            .warnings
            .iter()
            .any(|warning| warning.contains(".source-template")),
        "{:?}",
        kept.warnings
    );

    manager(&store)
        .remove(
            "x",
            &repo,
            ArchivedCheckpoints::Keep,
            SourceBranchPolicy::Keep,
        )
        .expect("remove");
    let cleaned = manager(&store)
        .cleanup(false, ArchivedCheckpoints::Keep)
        .expect("cleanup");
    assert!(cleaned.orphan_workspaces.contains(&template));
    assert!(!template.exists());
    assert!(!Utf8PathBuf::from(format!("{template}.lock")).exists());
}
