//! Trackers with `paths = []` — deposit-only ones, and ones that only bring
//! `resources` — through the CLI.
//!
//! #246: such a tracker could be bound but never dropped, because `drop` (and
//! `use` on a role alternative, which drops the outgoing tracker) captured it
//! from the workspace first, and a tracker with no paths has nothing there.
//! `status` also listed every tracker in a role instead of the bound one.
//!
//! #247: `tracker list` left these trackers out entirely.
//!
//! The Supabase stack from the issue is stood in for by a `db` resource that
//! deposits into `branch-db` at checkpoint, and a long-running `functions`.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
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

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "newgit-pathless-trackers-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

const DB_RESOURCE: &str = r#"ownership = "branch"

[actions.prepare]
command = "true"

[checkpoint]
mode = "command"
command = "echo rows > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "branch-db"
"#;

const FUNCTIONS_RESOURCE: &str = r#"ownership = "branch"

[actions.start]
command = "sleep 30"
long_running = true

[actions.stop]
signal = "term"
"#;

/// The issue's `branch-db`: no paths, opted into, brings the stack.
const BRANCH_DB: &str = r#"audience = "user"
storage = "local"
paths = []
priority = -1
resources = ["db", "functions"]
"#;

fn setup(temp: &Utf8Path, trackers: &[(&str, &str)]) -> Utf8PathBuf {
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
    for (name, contents) in [("db", DB_RESOURCE), ("functions", FUNCTIONS_RESOURCE)] {
        std::fs::write(
            store.paths().resources.join(format!("{name}.toml")),
            contents,
        )
        .expect("write resource");
    }
    for (name, contents) in trackers {
        std::fs::write(
            store.paths().trackers.join(format!("{name}.toml")),
            contents,
        )
        .expect("write tracker");
    }
    repo
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

fn newgit_ok(repo: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(repo, args);
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

/// The TRACKERS cell of `status <instance>`, split into tracker names.
fn status_trackers(repo: &Utf8Path, instance: &str) -> Vec<String> {
    let stdout = newgit_ok(repo, &["status", instance]);
    let mut lines = stdout.lines();
    let header = lines.next().expect("header");
    let start = header.find("TRACKERS").expect("TRACKERS column");
    let end = header.find("RESOURCES").expect("RESOURCES column");
    let row = lines
        .find(|line| line.split_whitespace().next() == Some(instance))
        .unwrap_or_else(|| panic!("no row for {instance}:\n{stdout}"));
    row[start..end.min(row.len())]
        .split_whitespace()
        .filter(|cell| *cell != "-")
        .map(|cell| cell.split(':').next().unwrap_or(cell).to_owned())
        .collect()
}

#[test]
fn a_tracker_with_no_paths_can_be_used_and_dropped() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp, &[("branch-db", BRANCH_DB)]);

    newgit_ok(&repo, &["spawn", "try-db"]);
    assert!(!status_trackers(&repo, "try-db").contains(&"branch-db".to_owned()));

    let used = newgit_ok(&repo, &["tracker", "use", "branch-db", "try-db"]);
    assert!(
        used.contains("bound what it brings: db, functions"),
        "{used}"
    );
    newgit_ok(&repo, &["checkpoint", "try-db", "--verify"]);
    assert!(status_trackers(&repo, "try-db").contains(&"branch-db".to_owned()));

    let dropped = newgit_ok(&repo, &["tracker", "drop", "branch-db", "try-db"]);
    assert!(
        dropped.contains("parked `db`") && dropped.contains("parked `functions`"),
        "{dropped}"
    );
    assert!(
        !dropped.contains("captured"),
        "nothing in the workspace to capture: {dropped}"
    );
    assert!(!status_trackers(&repo, "try-db").contains(&"branch-db".to_owned()));

    // `--stash` and `--discard` have nothing to act on, and do not fail.
    for flag in ["--stash", "--discard"] {
        newgit_ok(&repo, &["tracker", "use", "branch-db", "try-db"]);
        let dropped = newgit_ok(&repo, &["tracker", "drop", flag, "branch-db", "try-db"]);
        assert!(dropped.contains("Dropped `branch-db`"), "{flag}: {dropped}");
    }

    // An explicit capture is a direct request, so it still says why not.
    newgit_ok(&repo, &["tracker", "use", "branch-db", "try-db"]);
    let capture = newgit(&repo, &["tracker", "capture", "branch-db", "try-db"]);
    assert!(!capture.status.success());
    assert!(
        String::from_utf8_lossy(&capture.stderr).contains("owns no paths"),
        "{}",
        String::from_utf8_lossy(&capture.stderr)
    );
    newgit_ok(&repo, &["tracker", "drop", "branch-db", "try-db"]);
}

#[test]
fn role_alternatives_without_paths_swap_and_status_shows_the_bound_one() {
    let (_guard, temp) = tempdir();
    let shared_db = "audience = \"user\"\nstorage = \"local\"\npaths = []\nrole = \"db\"\n";
    let branch_db = "audience = \"user\"\nstorage = \"local\"\npaths = []\nrole = \"db\"\n\
                     priority = -1\nresources = [\"db\", \"functions\"]\n";
    let repo = setup(&temp, &[("shared-db", shared_db), ("branch-db", branch_db)]);

    newgit_ok(&repo, &["spawn", "try-shared"]);
    assert_eq!(status_trackers(&repo, "try-shared"), ["shared-db"]);

    let used = newgit_ok(&repo, &["tracker", "use", "branch-db", "try-shared"]);
    assert!(
        used.contains("dropped `shared-db`, which filled the same role"),
        "{used}"
    );
    assert_eq!(status_trackers(&repo, "try-shared"), ["branch-db"]);

    let back = newgit_ok(&repo, &["tracker", "use", "shared-db", "try-shared"]);
    assert!(back.contains("dropped `branch-db`"), "{back}");
    assert_eq!(status_trackers(&repo, "try-shared"), ["shared-db"]);
}

#[test]
fn tracker_list_shows_trackers_with_no_paths_and_their_roles() {
    let (_guard, temp) = tempdir();
    let shared_db = "audience = \"user\"\nstorage = \"local\"\npaths = []\nrole = \"db\"\n";
    let branch_db = "audience = \"user\"\nstorage = \"local\"\npaths = []\nrole = \"db\"\n\
                     priority = -1\nresources = [\"db\", \"functions\"]\n";
    let bare = "audience = \"user\"\nstorage = \"local\"\npaths = []\n";
    let repo = setup(
        &temp,
        &[
            ("shared-db", shared_db),
            ("branch-db", branch_db),
            ("bare", bare),
        ],
    );

    let stdout = newgit_ok(&repo, &["tracker", "list"]);
    let row = |name: &str| -> String {
        stdout
            .lines()
            .find(|line| line.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("no `{name}` row:\n{stdout}"))
            .to_owned()
    };
    let branch = row("branch-db");
    assert!(branch.contains("db (-1)"), "{branch}");
    assert!(
        branch.ends_with("(no paths; deposits from db; brings db, functions)"),
        "{branch}"
    );
    let shared = row("shared-db");
    assert!(shared.contains(" db "), "{shared}");
    assert!(shared.ends_with("(no paths)"), "{shared}");
    assert!(row("bare").ends_with("(no paths)"));
}
