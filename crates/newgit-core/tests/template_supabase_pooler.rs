//! #205: the `supabase` template's committed `config.toml` names a pooler
//! port the template did not allocate, so a second instance's port scan could
//! land on it and the render refused as non-invertible. Every port literal the
//! committed file names is now one the template allocates and renders, so no
//! scan outcome can collide with a line the render leaves alone.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

/// The port lines `supabase init` writes, including the pooler's.
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

/// Every `port`-suffixed key in `config`, by `(section, key)`.
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

/// The second instance's scan starts above the first's ports, which on an
/// idle machine put one of them on the committed pooler line. Both renders
/// have to succeed whatever the machine is listening on.
#[test]
fn a_second_supabase_instance_renders_past_the_committed_pooler_port() {
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

    // The pooler is part of the stack, so it moves with the rest: an enabled
    // pooler on the second instance must not want the first's port.
    let a = &first.branch.resources["supabase"];
    let b = &second.branch.resources["supabase"];
    assert_ne!(a.resolved_ports["pooler"], b.resolved_ports["pooler"]);

    let rendered =
        std::fs::read_to_string(second.branch.workspace_path.join("supabase/config.toml"))
            .expect("read rendered config");
    let rendered_ports = parse_ports(&rendered);
    assert_eq!(
        rendered_ports
            .get(&("db.pooler".to_owned(), "port".to_owned()))
            .copied(),
        Some(b.resolved_ports["pooler"]),
        "[db.pooler] port did not render to the allocated `pooler`:\n{rendered}"
    );

    // Every port the file names is one the binding record allocated, so no
    // future scan can collide with a literal the render leaves alone.
    let allocated: std::collections::BTreeSet<u16> = b.resolved_ports.values().copied().collect();
    for ((section, key), port) in &rendered_ports {
        assert!(
            allocated.contains(port),
            "[{section}] {key} = {port} is not a port the template allocates"
        );
    }
}
