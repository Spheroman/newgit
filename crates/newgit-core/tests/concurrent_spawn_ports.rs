//! Two instances spawned at the same time are not promised the same port.
//! newgit exists to run many agents in parallel, each with its own instance,
//! so two `newgit spawn`s overlapping is the expected case.
//!
//! The binding records are the only claim on a port. `bind_resources` used
//! to read them, allocate, and run `prepare`, and `spawn` wrote the new
//! record only after that: a spawn that started while another's `prepare`
//! was running saw no claim on the port the first had been given, the bind
//! probe saw nothing listening on it either, and both were promised it. Now
//! a resource's ports go into the record, under the store's port lock,
//! before its `prepare` runs.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::{BranchInstance, ResourceBinding, ResourceStatus};
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

/// Spawn `name` with a manager of its own, as a second `newgit` process would,
/// and return the port it was promised.
fn spawn_port(root: &Utf8Path, name: &str) -> u16 {
    let manager = BranchManager::open(MetadataStore::at(root)).expect("manager");
    let spawned = manager.spawn(name, None).expect("spawn");
    spawned.branch.resources["web"].resolved_ports["http"]
}

/// The overlap a slow install gives two real agents, pinned rather than left
/// to timing: `b` starts once `a` is inside its `prepare`, and `a`'s
/// `prepare` waits (up to 20 s) until `b` has reached its own. So this is
/// deterministic, not `#[ignore]`d. `a`'s `prepare` fails if `b` never gets
/// there, which is what a lock held across `prepare` would do: it would keep
/// the ports apart by making each spawn wait for the one before it to finish
/// installing.
#[test]
fn two_concurrent_spawns_are_not_promised_the_same_port() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    let markers = temp.join("markers");
    std::fs::create_dir_all(&markers).expect("mkdir");

    // One port, and a `prepare` that marks when it starts. `a`'s then waits
    // for `b`'s mark, and fails if it never comes; `b`'s finds its own and
    // returns at once.
    write_resource(
        &store,
        "web",
        &format!(
            r#"ownership = "branch"

[ports]
http = {{ start = 47600 }}

[actions.prepare]
command = "touch {markers}/{{{{branch.slug}}}}.started; i=0; while [ ! -f {markers}/b.started ] && [ $i -lt 200 ]; do sleep 0.1; i=$((i+1)); done; [ -f {markers}/b.started ]"
"#
        ),
    );

    let a = std::thread::spawn({
        let root = root.clone();
        move || spawn_port(&root, "a")
    });
    let b = std::thread::spawn({
        let root = root.clone();
        let started = markers.join("a.started");
        move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !started.exists() {
                assert!(Instant::now() < deadline, "`a` never reached its prepare");
                std::thread::sleep(Duration::from_millis(20));
            }
            spawn_port(&root, "b")
        }
    });
    let a_port = a.join().expect("spawn a");
    let b_port = b.join().expect("spawn b");

    let branches = store.load_branches().expect("load branches");
    let claims: Vec<(String, u16)> = branches
        .iter()
        .map(|branch| {
            (
                branch.name.clone(),
                branch.resources["web"].resolved_ports["http"],
            )
        })
        .collect();
    assert_ne!(
        a_port, b_port,
        "two overlapping spawns were both promised port {a_port}; the binding records now \
         claim {claims:?}"
    );

    // Keeping them apart did not take the overlap away: `a`'s `prepare` saw
    // `b` reach its own.
    let statuses: Vec<(String, ResourceStatus)> = branches
        .iter()
        .map(|branch| (branch.name.clone(), branch.resources["web"].status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("a".to_owned(), ResourceStatus::Ready),
            ("b".to_owned(), ResourceStatus::Ready),
        ],
        "`b` did not reach its `prepare` while `a`'s was running, so the second spawn waited \
         for the first one's"
    );
}

/// What the lock itself closes: the moment between one spawn reading the
/// records and writing its claim, when another reading them would miss it.
/// The test plays the other newgit, holding the port lock while it writes a
/// claim on 47700 as `claim_ports` does. `a`, spawned meanwhile, may read the
/// records only once the lock is let go, so it is given another port. (Its
/// own range, so the probes of the test above cannot move it off 47700.)
#[test]
fn a_claim_written_under_the_port_lock_is_seen_by_the_next_spawn() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let root = store.paths().project_root.clone();
    write_resource(
        &store,
        "web",
        r#"ownership = "branch"

[ports]
http = { start = 47700 }
"#,
    );

    let lock = store.lock_ports().expect("take the port lock");
    let workspace = temp.join("workspaces/a");
    let a = std::thread::spawn(move || spawn_port(&root, "a"));
    // From its workspace to its ports takes `a` a fraction of a second, so
    // give it that and more. Nothing depends on it while `a` takes the lock,
    // since `a` cannot allocate before the lock is let go; it is what lets
    // this catch a spawn that does not take it.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !workspace.exists() {
        assert!(Instant::now() < deadline, "`a` never got a workspace");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_secs(1));
    let mut other = BranchInstance::new(
        "other",
        "other",
        "0".repeat(40),
        temp.join("workspaces/other"),
    )
    .expect("instance");
    other.resources.insert(
        "web".to_owned(),
        ResourceBinding {
            definition_rev: String::new(),
            resolved_ports: BTreeMap::from([("http".to_owned(), 47700)]),
            resolved_exports: BTreeMap::new(),
            rendered: Vec::new(),
            status: ResourceStatus::Pending,
            restore_proven: false,
            render_error: None,
            export_error: None,
        },
    );
    store
        .create_branch_record(&other)
        .expect("write the other claim");
    drop(lock);

    assert_ne!(
        a.join().expect("spawn a"),
        47700,
        "`a` was promised the port another spawn claimed while it held the port lock"
    );
}
