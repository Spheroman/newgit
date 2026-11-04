//! Pins issue #59: within one resource, ports are allocated in name order
//! (the `[ports]` table is a `BTreeMap`, iterated by key), which is also the
//! order `newgit resource list` and friends display them in. This is not
//! obvious from a TOML table, whose declaration order looks like it could
//! matter instead — this test proves it does not.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

/// `zeta` is declared first and reaches into the range `alpha` would
/// otherwise take, and both start on the same port. If allocation followed
/// declaration order, `zeta` would claim the first free port from that start
/// and push `alpha` past it. Name order (what `BTreeMap` iteration gives)
/// means `alpha` is allocated first instead, so `alpha` takes that port and
/// `zeta` is the one pushed up.
const OVERLAPPING_RESOURCE: &str = r#"ownership = "branch"

[ports]
zeta = { start = 48200 }
alpha = { start = 48200 }
"#;

#[test]
fn ports_within_one_resource_are_allocated_in_name_order() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", OVERLAPPING_RESOURCE);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");

    let spawned = manager.spawn("feature", None).expect("spawn");
    let ports = &spawned.branch.resources["app"].resolved_ports;

    // Name-sorted allocation: `alpha` is considered before `zeta`, so
    // `alpha` gets the first free port from the shared start and `zeta` is
    // scanned past it.
    //
    // This compares the two ports instead of expecting 48200 and 48201.
    // `allocate` skips any port it cannot bind with `SO_REUSEADDR` off
    // (#93), so a port held by anything else, even a client socket an
    // earlier test left in `TIME_WAIT`, pushes the allocation past it. 48200
    // is inside Linux's ephemeral range (32768-60999), where the kernel picks
    // client ports, and on ubuntu-latest CI `alpha` once got 48201. Moving
    // the start below that range would still need two particular ports
    // free. Both scans start on the same port and go up, so whichever is
    // allocated first gets the lower port, wherever the free ones are.
    assert!(
        ports["alpha"] < ports["zeta"],
        "alpha is allocated first (name order), so zeta is scanned past it: {ports:?}"
    );
}
