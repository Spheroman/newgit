//! Regression test for #44: `resource list` and `tracker list` compute
//! column widths from every row, not a hardcoded guess. A long value in one
//! row (a `PROFILE` with several traits, an `AUDIENCE` longer than the
//! default) must not push the columns after it out of alignment for every
//! other row.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::Command;

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

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops. Hand-rolled rather than pulling in the `tempfile`
/// dev-dependency other crates use, since this crate has no existing test
/// infrastructure and a table-alignment fix is not a reason to add one.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("newgit-table-alignment-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

fn write_tracker(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().trackers.join(format!("{name}.toml")),
        contents,
    )
    .expect("write tracker");
}

fn run_newgit(repo: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    assert!(
        output.status.success(),
        "newgit {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

/// Given a table's lines, the byte offset at which a named column starts is
/// found from the header. Every data row must start its own value for that
/// column at the exact same offset, or the columns have drifted.
fn column_start(header: &str, label: &str) -> usize {
    header.find(label).unwrap_or_else(|| {
        panic!("header {header:?} has no {label:?} column");
    })
}

const SHORT_RESOURCE: &str = r#"ownership = "branch"
"#;

// Mirrors the `supabase` row from #44: three profile traits (ports, render,
// checkpoint:command) make PROFILE longer than the old hardcoded width of 27.
const LONG_PROFILE_RESOURCE: &str = r#"ownership = "branch"

[ports]
api = { start = 54321 }

[[render]]
path = "config.toml"
replace = [
  { find = "port = 1", with = "port = {{ports.api}}" },
]

[checkpoint]
mode = "command"
command = "echo checkpoint"
"#;

#[test]
fn resource_list_columns_stay_aligned_when_a_profile_is_long() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    // `a-short` sorts before `z-supabase`, so the long PROFILE row is not
    // last — a bug that only misaligns the very last column would pass a
    // test that only checks the tail.
    write_resource(&store, "a-short", SHORT_RESOURCE);
    write_resource(&store, "z-supabase", LONG_PROFILE_RESOURCE);
    let repo = store.paths().project_root.clone();

    let stdout = run_newgit(&repo, &["resource", "list"]);
    let lines: Vec<&str> = stdout.lines().collect();
    let header = lines[0];
    assert!(header.starts_with("NAME"), "unexpected header: {header:?}");

    let ownership_col = column_start(header, "OWNERSHIP");
    let depends_col = column_start(header, "DEPENDS_ON");
    let actions_col = column_start(header, "ACTIONS");

    let short_row = lines
        .iter()
        .find(|line| line.starts_with("a-short"))
        .expect("a-short row present");
    let long_row = lines
        .iter()
        .find(|line| line.starts_with("z-supabase"))
        .expect("z-supabase row present");

    // The long PROFILE value must have widened every column, and both rows
    // must agree on where OWNERSHIP, DEPENDS_ON, and ACTIONS start.
    for row in [short_row, long_row] {
        assert_eq!(
            &row[ownership_col..ownership_col + "branch".len()],
            "branch",
            "OWNERSHIP misaligned in row {row:?} (expected at column {ownership_col})"
        );
        assert_eq!(
            &row[depends_col..depends_col + 1],
            "-",
            "DEPENDS_ON misaligned in row {row:?} (expected at column {depends_col})"
        );
    }
    assert!(
        long_row.len() >= actions_col,
        "ACTIONS column pushed off the long row: {long_row:?}"
    );
}

const SHORT_TRACKER: &str = r#"audience = "user"
storage = "local"
paths = ["src"]
"#;

// A path list long enough to widen PATHS is not what drifts AUDIENCE — the
// bug is a hardcoded width on AUDIENCE itself. `project-devs` is already
// longer than the old hardcoded width of 9.
const LONG_AUDIENCE_TRACKER: &str = r#"audience = "project-devs"
storage = "remote"
merge_with_source = true
paths = ["notes"]
"#;

#[test]
fn tracker_list_columns_stay_aligned_when_an_audience_is_long() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_tracker(&store, "a-short", SHORT_TRACKER);
    write_tracker(&store, "z-long", LONG_AUDIENCE_TRACKER);
    let repo = store.paths().project_root.clone();

    let stdout = run_newgit(&repo, &["tracker", "list"]);
    let lines: Vec<&str> = stdout.lines().collect();
    let header = lines[0];
    assert!(header.starts_with("NAME"), "unexpected header: {header:?}");

    let storage_col = column_start(header, "STORAGE");
    let audience_col = column_start(header, "AUDIENCE");
    let paths_col = column_start(header, "PATHS");

    let short_row = lines
        .iter()
        .find(|line| line.starts_with("a-short"))
        .expect("a-short row present");
    let long_row = lines
        .iter()
        .find(|line| line.starts_with("z-long"))
        .expect("z-long row present");

    assert_eq!(
        &short_row[storage_col..storage_col + "local".len()],
        "local"
    );
    assert_eq!(
        &long_row[storage_col..storage_col + "remote".len()],
        "remote"
    );
    assert!(
        short_row[audience_col..].starts_with("user"),
        "AUDIENCE misaligned in row {short_row:?} (expected at column {audience_col})"
    );
    assert!(
        long_row[paths_col..].starts_with("notes"),
        "PATHS misaligned in row {long_row:?} (expected at column {paths_col})"
    );
}

// Mirrors #44's `db-snapshots` shape: a resource whose checkpoint deposits
// into a tracker that owns no workspace paths of its own.
const DB_TRACKER_DEPOSIT_RESOURCE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "echo dump-data > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "supabase-db"

[restore]
mode = "command"
command = "cp {{state_ref}} restored.sql"
"#;

/// A tracker with no `paths` of its own exists only to receive
/// `into_tracker` deposits (see #44). Once a checkpoint has deposited
/// content, its rev shows up on every later `spawn` and `undo`; the bind
/// line should say `deposit-only` instead of reading like a checkout that
/// silently found nothing.
#[test]
fn undo_and_later_spawns_report_deposit_only_trackers_by_name_not_a_zero_count() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "db", DB_TRACKER_DEPOSIT_RESOURCE);
    write_tracker(
        &store,
        "supabase-db",
        r#"audience = "user"
storage = "local"
paths = []
"#,
    );
    let repo = store.paths().project_root.clone();

    run_newgit(&repo, &["spawn", "feature-a"]);
    run_newgit(&repo, &["checkpoint", "feature-a"]);

    let undo_stdout = run_newgit(&repo, &["undo", "feature-a"]);
    let undo_line = undo_stdout
        .lines()
        .find(|line| line.contains("supabase-db"))
        .expect("tracker line present in undo output");
    assert!(
        undo_line.contains("deposit-only"),
        "expected a deposit-only line on undo, got: {undo_line:?}"
    );
    assert!(
        !undo_line.contains("0 files"),
        "a deposit-only tracker should not read like a failed restore: {undo_line:?}"
    );

    // A deposit lands on the branch's own tracker binding, not the lane
    // head; `tracker merge` promotes it so a *new* instance can bind it too
    // — same papercut, same fix, on the path a real user actually takes.
    run_newgit(&repo, &["tracker", "merge", "supabase-db", "feature-a"]);
    let spawn_stdout = run_newgit(&repo, &["spawn", "feature-b"]);
    let spawn_line = spawn_stdout
        .lines()
        .find(|line| line.contains("supabase-db"))
        .expect("tracker line present in spawn output");
    assert!(
        spawn_line.contains("deposit-only"),
        "expected a deposit-only line on spawn, got: {spawn_line:?}"
    );
    assert!(
        !spawn_line.contains("0 files"),
        "a deposit-only tracker should not read like a failed checkout: {spawn_line:?}"
    );
}
