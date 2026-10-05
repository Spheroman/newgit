//! `checkpoint --verify` on a mismatch: say which half of the round trip
//! broke — `[restore]` or `[checkpoint]` — and show what differs between the
//! two deposits instead of two opaque revs (#79).

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::deposit_diff::FileChange;
use newgit_core::manager::{BranchManager, DiffPair, MismatchCause, VerifyResource};
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

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

/// A project with a deposit-only `db-snapshots` lane and one resource `db`,
/// spawned as `feature`. The resource's state lives in `<temp>/db.txt`,
/// outside the workspace, so only its own `[restore]` can put it back.
fn spawned(temp: &Utf8Path, resource: &str) -> BranchManager {
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

    let rows: String = (0..40).map(|n| format!("row {n}\n")).collect();
    std::fs::write(temp.join("db.txt"), rows).expect("write db");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        resource.replace("STATE", temp.join("db.txt").as_str()),
    )
    .expect("write resource");

    let m = BranchManager::open(store).expect("manager");
    m.create_tracker("db-snapshots", "project-devs", Storage::Local, false)
        .expect("create tracker");
    let m = BranchManager::open(m.store().clone()).expect("manager");
    m.spawn("feature", None).expect("spawn");
    m
}

fn verify_db(m: &BranchManager) -> VerifyResource {
    let outcome = m
        .checkpoint_verify("feature", None)
        .expect("verify runs even when it finds a mismatch");
    assert!(!outcome.is_proven(), "a mismatch is never verified");
    let resource = outcome.resources[0].clone();
    assert!(resource.exercised && !resource.agree);
    resource
}

/// `pg_dump`'s `\restrict <random token>` preamble, reduced: every dump of
/// the same state differs in exactly one line. `$$` is the shell's pid,
/// new on every run.
const NONCE_DUMP: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
# `printf`, not `echo`: some `sh` echoes turn `\r` into a carriage return.
command = '''printf '\\restrict %s\n' $$ > {{snapshot.path}}/db.sql && cat STATE >> {{snapshot.path}}/db.sql'''
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "true"
"#;

#[test]
fn a_nondeterministic_checkpoint_is_blamed_and_its_one_changed_line_shown() {
    let (_guard, temp) = tempdir();
    let m = spawned(&temp, NONCE_DUMP);

    let resource = verify_db(&m);
    assert!(
        matches!(
            &resource.cause,
            Some(MismatchCause::CheckpointUnstable { control_state_ref: Some(control) })
                if Some(control) != resource.after_state_ref.as_ref()
        ),
        "checkpointing twice with nothing restored in between differed, so the checkpoint is \
         at fault: {:?}",
        resource.cause
    );

    // Shown as after → control, not before → after: nothing was restored
    // between those two, so every line in it is the checkpoint's own noise.
    let shown = resource.diff.expect("both refs are deposits");
    assert_eq!(shown.pair, DiffPair::AfterControl);
    let diff = shown.result.expect("diffable");
    let (lane, after_rev) = resource
        .after_state_ref
        .as_deref()
        .and_then(|r| r.strip_prefix("tracker:"))
        .and_then(|r| r.split_once('@'))
        .expect("after is a deposit");
    assert_eq!(lane, "db-snapshots");
    assert!(diff.before.ends_with(after_rev), "{}", diff.before);
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "db.sql");
    assert_eq!(
        diff.files[0].change,
        FileChange::Text {
            removed: 1,
            added: 1,
            lines_before: 41
        }
    );
    assert_eq!(diff.excerpt.len(), 2);
    assert!(
        diff.excerpt[0].starts_with("-\\restrict "),
        "{:?}",
        diff.excerpt
    );
    assert!(
        diff.excerpt[1].starts_with("+\\restrict "),
        "{:?}",
        diff.excerpt
    );
    // Both snapshots are still where the full-diff command says they are.
    assert!(diff.before.join("db.sql").is_file());
    assert!(diff.after.join("db.sql").is_file());
    assert!(diff.full_diff_command().contains(diff.before.as_str()));

    // The control run gets its own log, named as the control, beside the
    // logs of the checkpoints it is compared against.
    let store = m.store();
    let slug = store.find_branch("feature").expect("branch").slug;
    let logs: Vec<String> = std::fs::read_dir(store.paths().logs.join(&slug))
        .expect("logs dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf8")
        })
        .collect();
    let control = logs
        .iter()
        .filter(|name| name.starts_with("db.checkpoint-control-"))
        .count();
    let checkpoints = logs
        .iter()
        .filter(|name| name.starts_with("db.checkpoint-") && !name.contains("control"))
        .count();
    assert_eq!(control, 1, "{logs:?}");
    assert!(
        checkpoints >= 2,
        "before and after both keep a log: {logs:?}"
    );
}

/// A stable dump, and a restore that exits 0 having written the wrong
/// state — the failure `--verify` exists to catch.
const WRONG_RESTORE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "cp STATE {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "echo truncated > STATE"
"#;

#[test]
fn a_restore_that_lands_elsewhere_is_blamed_when_the_checkpoint_reproduces_itself() {
    let (_guard, temp) = tempdir();
    let m = spawned(&temp, WRONG_RESTORE);

    let resource = verify_db(&m);
    assert_eq!(resource.cause, Some(MismatchCause::RestoreLandedElsewhere));
    let shown = resource.diff.expect("both refs are deposits");
    assert_eq!(shown.pair, DiffPair::BeforeAfter);
    let diff = shown.result.expect("diffable");
    assert_eq!(
        diff.files[0].change,
        FileChange::Text {
            removed: 40,
            added: 1,
            lines_before: 40
        }
    );
    assert!(diff.excerpt_omitted > 0, "40 removed lines do not all fit");
}

const FAILING_RESTORE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "cp STATE {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "echo half-loaded > STATE && exit 1"
"#;

#[test]
fn a_restore_that_failed_outright_is_named_without_a_control_checkpoint() {
    let (_guard, temp) = tempdir();
    let m = spawned(&temp, FAILING_RESTORE);

    let resource = verify_db(&m);
    assert_eq!(resource.cause, Some(MismatchCause::RestoreFailed));
}

/// No deposit, no diff: an opaque command ref is all the content there is.
const OPAQUE_NONCE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "echo snapshot-$$"

[restore]
mode = "command"
command = "true"
"#;

#[test]
fn an_opaque_ref_still_gets_a_cause_but_no_diff() {
    let (_guard, temp) = tempdir();
    let m = spawned(&temp, OPAQUE_NONCE);

    let resource = verify_db(&m);
    assert!(matches!(
        resource.cause,
        Some(MismatchCause::CheckpointUnstable { .. })
    ));
    assert_eq!(resource.diff, None);
}
