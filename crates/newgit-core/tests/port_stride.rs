//! Instances are allocated 100 apart, not 1 (`ports::PORT_STRIDE`). Found
//! by an agent running three instances of a Remix app: newgit handed out
//! 3001, 3002 and 3003, `remix dev` picks a second port for itself by
//! scanning 3001-3100, and the first instance's tool took 3002 before the
//! second instance's server bound it. A claim holds nothing until its owner
//! binds; only the spacing keeps another instance's claim out of that scan.
//!
//! Every start here is a port the OS just handed out, and the assertions
//! are about distances from it, never about a fixed port being free.

use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::ports::{PORT_STRIDE, allocate};
use newgit_core::store::MetadataStore;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("an OS-assigned port")
        .port()
}

/// A start low enough that a few strides above it stay in range.
fn start() -> u16 {
    loop {
        let port = free_port();
        if port < u16::MAX - 10 * PORT_STRIDE {
            return port;
        }
    }
}

fn steps_above(start: u16, port: u16) -> u16 {
    assert!(port >= start, "{port} is below its start {start}");
    assert_eq!(
        (port - start) % PORT_STRIDE,
        0,
        "{port} is not a whole number of strides above {start}"
    );
    (port - start) / PORT_STRIDE
}

#[test]
fn a_claimed_start_moves_a_whole_stride_up() {
    let start = start();
    let mut used = BTreeSet::from([start]);
    let port = allocate(start, &mut used).expect("allocate");
    assert!(steps_above(start, port) >= 1, "{port}");
    assert!(used.contains(&port));

    // And the next one past that, not the port beside it.
    let next = allocate(start, &mut used).expect("allocate");
    assert!(
        steps_above(start, next) > steps_above(start, port),
        "{next}"
    );
}

/// A port some other program holds is passed over the same way: the
/// candidates are only ever `start + k * 100`.
#[test]
fn a_held_candidate_is_skipped_by_a_stride() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let start = held.local_addr().expect("addr").port();
    if start > u16::MAX - 10 * PORT_STRIDE {
        return;
    }
    let port = allocate(start, &mut BTreeSet::new()).expect("allocate");
    assert!(steps_above(start, port) >= 1, "{port}");
}

fn git(dir: &Utf8Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// Three spawns of one resource with two ports: each port lands a whole
/// number of strides above its own `start`, never on the port beside a
/// claim, and no two instances share one.
#[test]
fn spawned_instances_sit_whole_strides_apart() {
    let temp = tempfile::tempdir().expect("tempdir");
    let temp = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8");
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

    let start = start();
    std::fs::write(
        store.paths().resources.join("app.toml"),
        format!(
            "ownership = \"branch\"\n\n[ports]\napi = {{ start = {start} }}\ndb = {{ start = {} }}\n",
            start + 1
        ),
    )
    .expect("write resource");
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("manager");

    let mut seen = BTreeSet::new();
    for name in ["a", "b", "c"] {
        let spawned = manager.spawn(name, None).expect("spawn");
        let ports = &spawned.branch.resources["app"].resolved_ports;
        let api = steps_above(start, ports["api"]);
        steps_above(start + 1, ports["db"]);
        assert!(
            seen.insert(api),
            "`{name}` was handed a taken api port: {ports:?}"
        );
    }
}
