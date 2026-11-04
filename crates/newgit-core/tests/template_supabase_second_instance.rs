//! #205: the `supabase` template's committed `config.toml` names ports the
//! template did not allocate (the pooler's, and the commented-out
//! `smtp_port` and `pop3_port`), so a second instance's port scan could land
//! on one and the render refused as non-invertible. Every port literal in
//! that block is now one the template allocates and renders, so no scan
//! outcome can collide with a line the render leaves alone.

use std::collections::BTreeSet;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

mod common;
use common::add_supabase_without_starting_it;

/// `supabase init` output from CLI 2.111.0, verbatim apart from `project_id`.
/// A hand-trimmed fixture is how #205 survived its first fix: it left out the
/// commented `# smtp_port = 54325`, which `port = 54325` matches inside.
const CONFIG_TOML: &str = include_str!("fixtures/supabase-init-config.toml");

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

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("supabase")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("supabase/config.toml"), CONFIG_TOML).expect("write config");
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

/// Every five-digit number in `text`, which in a Supabase config is a port.
fn five_digit_literals(text: &str) -> Vec<u16> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|token| token.len() == 5)
        .filter_map(|token| token.parse().ok())
        .collect()
}

/// Every port the committed file names is inside some `find`, so it moves
/// with the instance. Unlike the spawn test below, this does not depend on
/// what the machine is listening on.
#[test]
fn every_port_supabase_init_writes_is_one_the_template_renders() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    BranchManager::open(MetadataStore::at(repo.clone()))
        .expect("manager")
        .add_resource("supabase", "supabase")
        .expect("add supabase");

    let definitions = MetadataStore::at(repo)
        .load_resource_definitions()
        .expect("definitions");
    let finds: Vec<&str> = definitions
        .iter()
        .find(|definition| definition.name == "supabase")
        .expect("supabase defined")
        .render
        .iter()
        .flat_map(|spec| spec.replace.iter().map(|rule| rule.find.as_str()))
        .collect();

    let literals = five_digit_literals(CONFIG_TOML);
    assert!(!literals.is_empty(), "the fixture names no ports");
    for port in literals {
        assert!(
            finds.iter().any(|find| find.contains(&port.to_string())),
            "the committed config.toml names {port}, which no `find` covers"
        );
    }
}

/// The second instance's scan starts above the first's ports, which on an
/// idle machine put one of them on a committed line the template did not
/// render. Both renders have to succeed whatever the machine is listening on.
#[test]
fn a_second_supabase_instance_renders_against_supabase_init_output() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    add_supabase_without_starting_it(&repo);

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let first = manager.spawn("feature-a", None).expect("spawn a");
    let second = manager.spawn("feature-b", None).expect("spawn b");

    for spawned in [&first, &second] {
        let resource = spawned
            .resources
            .iter()
            .find(|resource| resource.name == "supabase")
            .expect("supabase bound");
        assert_eq!(
            resource.render_error, None,
            "`{}` refused the render with ports {:?}",
            spawned.branch.name, spawned.branch.resources["supabase"].resolved_ports
        );
        assert_eq!(resource.export_error, None);
    }

    // Every port either rendered file names is one its binding record
    // allocated, so no future scan can collide with a literal the render
    // leaves alone.
    for spawned in [&first, &second] {
        let ports = &spawned.branch.resources["supabase"].resolved_ports;
        let allocated: BTreeSet<u16> = ports.values().copied().collect();
        let rendered =
            std::fs::read_to_string(spawned.branch.workspace_path.join("supabase/config.toml"))
                .expect("read rendered config");
        for port in five_digit_literals(&rendered) {
            assert!(
                allocated.contains(&port),
                "`{}` renders {port}, which is not one of its ports {ports:?}",
                spawned.branch.name
            );
        }
    }
}
