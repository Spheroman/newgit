//! The `supabase` template's value is entirely in whether its `[[render]]`
//! `find` strings match a real `supabase/config.toml` exactly once each
//! (#90). A `find` that matches zero times refuses at spawn — loud, fine — but
//! a template whose strings never match is a template nobody can use, and
//! that is not something reading the TOML tells you. So this renders it
//! against the CLI's committed defaults and checks the result.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

/// The port lines `supabase init` writes, in the shape it writes them.
const CONFIG_TOML: &str = r#"project_id = "EDIT_ME_PROJECT"

[api]
enabled = true
port = 54321
schemas = ["public", "graphql_public"]

[db]
port = 54322
shadow_port = 54320
major_version = 15

[db.pooler]
enabled = false
port = 54329

[studio]
enabled = true
port = 54323
api_url = "http://127.0.0.1"

[inbucket]
enabled = true
port = 54324

[analytics]
enabled = true
port = 54327
backend = "postgres"

[auth]
site_url = "http://127.0.0.1:3000"
additional_redirect_urls = ["exp://127.0.0.1:8081"]
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

/// A store repo with a committed `supabase/config.toml`, which is what a
/// `[[render]]` requires: it substitutes into committed content.
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

/// `(section, key) -> port`, so an assertion can name the line it means
/// rather than substring-matching a number that appears in several places.
fn parse_ports(config: &str) -> std::collections::BTreeMap<(String, String), u16> {
    let mut ports = std::collections::BTreeMap::new();
    let mut section = String::new();
    for line in config.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        if key.ends_with("port")
            && let Ok(port) = value.parse::<u16>()
        {
            ports.insert((section.clone(), key.to_owned()), port);
        }
    }
    ports
}

#[test]
fn the_supabase_template_renders_against_the_clis_committed_defaults() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    BranchManager::open(MetadataStore::at(repo.clone()))
        .expect("manager")
        .add_resource("supabase", "supabase")
        .expect("add supabase");

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let binding = &spawned.branch.resources["supabase"];

    // Every `find` matched exactly once. A miss refuses the render and is
    // reported here rather than thrown, so assert on the error, not a panic.
    let resource = spawned
        .resources
        .iter()
        .find(|resource| resource.name == "supabase")
        .expect("supabase bound");
    assert_eq!(
        resource.render_error, None,
        "the template's find strings must match `supabase init` output"
    );
    assert_eq!(resource.export_error, None);

    let rendered =
        std::fs::read_to_string(spawned.branch.workspace_path.join("supabase/config.toml"))
            .expect("read rendered config");

    // project_id is what scopes every container and volume. Without a
    // per-instance value the second `supabase start` adopts the first
    // instance's stack, which is the failure this template exists to prevent.
    assert!(
        rendered.contains(r#"project_id = "EDIT_ME_PROJECT-feature-a""#),
        "project_id must carry the instance slug, got:\n{rendered}"
    );

    // All six ports moved to their allocated values. The allocated numbers
    // depend on what is listening on the machine, so assert against the
    // binding record rather than against literals — a suite running in
    // parallel legitimately pushes these around.
    let rendered_ports = parse_ports(&rendered);
    for (section, key, port_name) in [
        ("api", "port", "api"),
        ("db", "port", "db"),
        ("db", "shadow_port", "shadow"),
        ("studio", "port", "studio"),
        ("inbucket", "port", "inbucket"),
        ("analytics", "port", "analytics"),
    ] {
        assert_eq!(
            rendered_ports
                .get(&(section.to_owned(), key.to_owned()))
                .copied(),
            Some(binding.resolved_ports[port_name]),
            "[{section}] {key} did not render to the allocated `{port_name}`:\n{rendered}"
        );
    }

    // `shadow_port = 54320` must not have been eaten by the `port = 54320`
    // that is its own substring, and the pooler is not one of the six.
    assert_ne!(
        rendered_ports[&("db".to_owned(), "shadow_port".to_owned())],
        rendered_ports[&("db".to_owned(), "port".to_owned())],
        "shadow_port and port collided"
    );
    assert_eq!(
        rendered_ports[&("db.pooler".to_owned(), "port".to_owned())],
        54329,
        "the pooler port is not newgit's to rewrite"
    );

    // The exports the restore hook depends on resolved to real values.
    let exports = &binding.resolved_exports;
    assert_eq!(
        exports["SUPABASE_DB_CONTAINER"],
        "supabase_db_EDIT_ME_PROJECT-feature-a"
    );
    assert_eq!(
        exports["SUPABASE_API_URL"],
        format!("http://127.0.0.1:{}", binding.resolved_ports["api"])
    );
}

/// The smaller half of #90: `command-snapshot-migrations` named Supabase as
/// its audience and handed out `createdb`/`pg_dump`/`psql`, which a Supabase
/// user does not have against their database. "Ready to adapt" and "ready to
/// adapt *if your database is on the host*" are different claims, so every
/// template now states which host tools it assumes — and the spec says every,
/// so this checks every.
#[test]
fn every_template_states_the_host_tools_it_assumes() {
    for template in newgit_core::templates::RESOURCE_TEMPLATES {
        assert!(
            template.contents.contains("HOST TOOLS:"),
            "`{}` does not say what it assumes is installed",
            template.name
        );
    }
}

/// Two instances must not land on one stack. The ports are what newgit
/// allocates and the `project_id` is what scopes the containers; both have to
/// differ, or `supabase start` adopts the neighbour's stack.
#[test]
fn two_instances_get_different_ports_and_different_project_ids() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    BranchManager::open(MetadataStore::at(repo.clone()))
        .expect("manager")
        .add_resource("supabase", "supabase")
        .expect("add supabase");

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let first = manager.spawn("feature-a", None).expect("spawn a");
    let second = manager.spawn("feature-b", None).expect("spawn b");

    let a = &first.branch.resources["supabase"];
    let b = &second.branch.resources["supabase"];

    assert_ne!(
        a.resolved_exports["SUPABASE_PROJECT"],
        b.resolved_exports["SUPABASE_PROJECT"]
    );
    for port_name in ["api", "db", "shadow", "studio", "inbucket", "analytics"] {
        assert_ne!(
            a.resolved_ports[port_name], b.resolved_ports[port_name],
            "`{port_name}` was handed to both instances"
        );
    }
}
