/// Coverage for the generic `install` template (issue #60): unlike `pnpm`,
/// it depends on nothing and creates no companion, and its two variable
/// lines are `EDIT ME` placeholders rather than a guess at your package
/// manager. New tests live in their own file per AGENTS.md's parallel-branch
/// conventions, so this is not appended to `tests/resources.rs`.
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::templates::{RESOURCE_TEMPLATES, resource_template};

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

/// The template is listed and loadable like any other, and — unlike
/// `pnpm` — instantiating it creates nothing else: no companion resource,
/// no companion tracker, no ordered cleanup to learn about later.
#[test]
fn install_template_is_listed_and_creates_no_companions() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo.clone())).expect("manager");

    let template = resource_template("install").expect("install template listed");
    assert!(template.companions.is_empty());
    assert!(template.companion_trackers.is_empty());

    let outcome = manager
        .add_resource("deps", "install")
        .expect("add install");
    assert!(outcome.path.is_file());
    assert!(outcome.companions_created.is_empty());
    assert!(outcome.trackers_created.is_empty());

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let names: Vec<&str> = manager
        .resource_definitions()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert!(names.contains(&"deps"));
}

/// The two lines that vary by package manager (`identity.paths` and the
/// `prepare` command) are unmistakably placeholders, not a guess that
/// happens to be wrong for npm — the whole point raised in #60.
#[test]
fn install_template_marks_its_variable_lines_edit_me() {
    let template = resource_template("install").expect("install template listed");
    assert!(template.contents.contains("EDIT ME"));

    let path_line = template
        .contents
        .lines()
        .find(|line| line.trim_start().starts_with("paths ="))
        .expect("identity.paths line present");
    assert!(path_line.contains("EDIT ME"));

    let command_line = template
        .contents
        .lines()
        .find(|line| line.trim_start().starts_with("command ="))
        .expect("prepare command line present");
    assert!(command_line.contains("EDIT ME"));
}

/// `install` sits alongside the other three starters, findable the same way.
#[test]
fn install_template_appears_in_the_template_listing() {
    let names: Vec<&str> = RESOURCE_TEMPLATES.iter().map(|t| t.name).collect();
    assert!(names.contains(&"install"));
}
