//! What a template tells you to do to it has to work when you do it (#242,
//! #244): a `config.toml` missing a section fails the supabase bind in words
//! that explain the shared default, the `workdir` the supabase template
//! offers parses, and a `process` with its port section deleted binds.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::templates::{instantiate, resource_template};

mod common;
use common::add_supabase_without_starting_it;

/// A trimmed config: no `[inbucket]`, no `[analytics]`.
const TRIMMED_CONFIG: &str = r#"project_id = "EDIT_ME_PROJECT"

[api]
port = 54321

[db]
port = 54322
shadow_port = 54320

[studio]
port = 54323
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
    assert!(output.status.success(), "git {args:?} failed");
}

/// A store repo with one committed file at `config_path`.
fn setup(temp: &Utf8Path, config_path: &str, config: &str) -> MetadataStore {
    let repo = temp.join("store");
    let config_file = repo.join(config_path);
    std::fs::create_dir_all(config_file.parent().expect("parent")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(&config_file, config).expect("write config");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut cfg = store.load_config().expect("config");
    cfg.workspace = Some(newgit_core::config::WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&cfg).expect("write config");
    store
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

#[test]
fn a_config_missing_a_section_fails_the_bind_and_says_why() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp, "supabase/config.toml", TRIMMED_CONFIG);
    let repo = store.paths().project_root.clone();

    add_supabase_without_starting_it(&repo);

    let spawned = BranchManager::open(MetadataStore::at(repo))
        .expect("reopen")
        .spawn("feature-a", None)
        .expect("spawn");
    let resource = spawned
        .resources
        .iter()
        .find(|resource| resource.name == "supabase")
        .expect("supabase bound");
    let error = resource.render_error.as_deref().expect("render refuses");
    assert!(error.contains("port = 54324"), "names the string: {error}");
    assert!(
        error.contains("falls back to its own default") && error.contains("every instance shares"),
        "explains the shared default: {error}"
    );
}

#[test]
fn the_supabase_workdir_line_is_offered_and_parses_when_uncommented() {
    let template = resource_template("supabase").expect("template");
    assert!(template.contents.contains("# workdir = \"apps/db\""));
    assert!(
        template.contents.contains("the [[render]] along with it"),
        "the note about changing the render path travels with workdir"
    );

    let (_guard, temp) = tempdir();
    let store = setup(&temp, "apps/db/supabase/config.toml", TRIMMED_CONFIG);
    let uncommented = template
        .contents
        .replace("# workdir = \"apps/db\"", "workdir = \"apps/db\"");
    store
        .write_resource_file("supabase", &instantiate(&uncommented, "supabase"))
        .expect("write resource");
    BranchManager::open(MetadataStore::at(store.paths().project_root.clone()))
        .expect("the definition with workdir set loads");
}

#[test]
fn a_process_without_a_port_binds_once_its_port_sections_are_deleted() {
    let template = resource_template("process").expect("template");
    assert!(template.contents.contains("THE PORT IS OPTIONAL"));

    // Do what the comment says: drop `[ports]`, `[exports]` and `[[render]]`.
    let contents = template.contents;
    let ports = contents.find("\n[ports]").expect("[ports]");
    let actions = contents.find("\n[actions.start]").expect("[actions.start]");
    let exports = contents.find("\n[exports]").expect("[exports]");
    let stripped = format!("{}{}", &contents[..ports], &contents[actions..exports]);

    let (_guard, temp) = tempdir();
    let store = setup(&temp, "README.md", "hi\n");
    store
        .write_resource_file("worker", &instantiate(&stripped, "worker"))
        .expect("write resource");

    let spawned = BranchManager::open(MetadataStore::at(store.paths().project_root.clone()))
        .expect("manager")
        .spawn("feature-a", None)
        .expect("spawn");
    let worker = &spawned.branch.resources["worker"];
    assert!(worker.resolved_ports.is_empty());
    let outcome = spawned
        .resources
        .iter()
        .find(|resource| resource.name == "worker")
        .expect("bound");
    assert_eq!(outcome.render_error, None);
    assert_eq!(outcome.export_error, None);
}
