//! #89, after review: what a failed hook leaves is all an agent has to fix
//! it with. newgit keeps all of it that it controls: the log of every run
//! (a retry writes its own), what the command left in the workspace
//! (nothing newgit runs afterwards removes it), the whole command as the
//! command, and the input it was given. And what it shows on the terminal
//! of a failure holds no more than a success would have: not a capture's
//! value, not an export's.

use std::io::Write as _;
use std::process::{Command, Stdio};

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
    let dir = std::env::temp_dir().join(format!("newgit-evidence-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path, resource: &str) -> Utf8PathBuf {
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
    std::fs::write(store.paths().resources.join("db.toml"), resource).expect("write resource");
    repo
}

/// Run newgit with `input` on its stdin (closed when `None`).
fn newgit_with(
    repo: &Utf8Path,
    args: &[&str],
    input: Option<&str>,
) -> (Option<i32>, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("newgit runs");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    let output = child.wait_with_output().expect("newgit finishes");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String, String) {
    newgit_with(repo, args, None)
}

/// The log an action's last line names: `[newgit] `x.y` exit N; log: <path>`.
fn action_log(stderr: &str) -> Utf8PathBuf {
    stderr
        .lines()
        .find_map(|line| line.split("; log: ").nth(1))
        .map(|path| Utf8PathBuf::from(path.trim()))
        .unwrap_or_else(|| panic!("no log named in:\n{stderr}"))
}

/// A prepare that leaves something behind, says a lot, and fails. Its
/// `[cleanup]` would remove what it left, if anything ran it.
const LEAVES_EVIDENCE: &str = r#"ownership = "branch"

[actions.prepare]
command = "echo run >> left-behind.txt; seq 300; echo \"failed on run $(grep -c run left-behind.txt)\" >&2; exit 4"

[cleanup]
command = "rm -f left-behind.txt"
"#;

/// newgit destroys nothing a failed hook leaves. Each run writes its own
/// log, so a retry cannot truncate the one before it, and the failure
/// names its own; nothing runs `[cleanup]` or anything else after a
/// failure, so what the command left in the workspace is still there for
/// whoever looks. What newgit cannot keep is what the command itself tears
/// down before it exits (`supabase start`'s containers); the reference
/// says how to rerun such a tool with its own keep-it-up flag.
#[test]
fn a_failed_prepare_keeps_its_log_and_what_it_left_through_a_retry() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp, LEAVES_EVIDENCE);

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_ne!(code, Some(0), "{stdout}{stderr}");
    let first = stdout
        .split("prepare: FAILED (log: ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .map(Utf8PathBuf::from)
        .unwrap_or_else(|| panic!("spawn names the failed log:\n{stdout}"));
    assert!(stdout.contains("| failed on run 1"), "{stdout}");
    let first_text = std::fs::read_to_string(&first).expect("first log");
    assert!(first_text.contains("\n150\n"), "the log has all of it");
    assert!(first_text.contains("failed on run 1"), "{first_text}");

    // The retry is the one spawn's failure suggests, quiet so it reports
    // the way spawn did.
    let (code, _, stderr) = newgit(&repo, &["action", "db.prepare", "x", "--quiet"]);
    assert_eq!(code, Some(4), "{stderr}");
    assert!(stderr.contains("  | failed on run 2"), "{stderr}");
    let second = action_log(&stderr);
    assert_ne!(second, first, "a retry writes a log of its own");
    assert!(
        std::fs::read_to_string(&second)
            .expect("second log")
            .contains("failed on run 2")
    );

    // Nor does bringing the instance up again undo any of it.
    newgit(&repo, &["start", "x"]);

    assert_eq!(
        std::fs::read_to_string(&first).expect("first log survives"),
        first_text,
        "the first run's log is exactly as it was"
    );
    let (code, left, stderr) = newgit(&repo, &["run", "x", "--", "cat", "left-behind.txt"]);
    assert_eq!(
        code,
        Some(0),
        "what the failed runs left is still there: {stderr}"
    );
    assert_eq!(left, "run\nrun\n", "both runs' leftovers, untouched");
}

/// A multi-line command is printed whole, as the command, and none of it
/// in the `|` lines that are the log's output.
#[test]
fn a_multi_line_command_prints_whole_as_the_command() {
    let (_guard, temp) = tempdir();
    let repo = setup(
        &temp,
        r#"ownership = "branch"

[actions.migrate]
command = """
echo step-one
echo step-two
echo migration-failed >&2
exit 2
"""
"#,
    );
    let (code, _, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stderr}");

    let (code, _, stderr) = newgit(&repo, &["action", "db.migrate", "x", "-q"]);
    assert_eq!(code, Some(2), "{stderr}");
    let indent = " ".repeat("[newgit] ran: ".len());
    assert!(
        stderr.contains(&format!(
            "[newgit] ran: echo step-one\n{indent}echo step-two\n{indent}echo migration-failed >&2\n{indent}exit 2\n"
        )),
        "{stderr}"
    );
    let tail: Vec<&str> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("  | "))
        .collect();
    assert_eq!(
        tail,
        ["step-one", "step-two", "migration-failed"],
        "only output in the tail: {stderr}"
    );

    // Spawn's own report of a failed prepare says it the same way.
    let (_guard, temp) = tempdir();
    let repo = setup(
        &temp,
        "ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"\"\"\necho one\nexit 5\n\"\"\"\n",
    );
    let (code, stdout, _) = newgit(&repo, &["spawn", "x"]);
    assert_ne!(code, Some(0));
    let indent = " ".repeat("               ran: ".len());
    assert!(
        stdout.contains(&format!("ran: echo one\n{indent}exit 5\n")),
        "{stdout}"
    );
    assert!(!stdout.contains("| exit 5"), "{stdout}");
}

/// `--quiet` moves output; the input newgit was given still reaches the
/// command, with captures or without.
#[test]
fn stdin_reaches_a_quiet_action() {
    let (_guard, temp) = tempdir();
    let repo = setup(
        &temp,
        r#"ownership = "branch"

[actions.ask]
command = "read answer && [ \"$answer\" = yes ]"

[actions.mint]
command = "read answer && echo TOKEN=$answer"
captures = ["TOKEN"]
"#,
    );
    let (code, _, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stderr}");

    for quiet in [false, true] {
        let mut args = vec!["action", "db.ask", "x"];
        if quiet {
            args.push("--quiet");
        }
        let (code, stdout, stderr) = newgit_with(&repo, &args, Some("yes\n"));
        assert_eq!(code, Some(0), "quiet={quiet}: {stdout}{stderr}");
    }

    let (code, stdout, stderr) =
        newgit_with(&repo, &["action", "db.mint", "x", "-q"], Some("minted\n"));
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (_, env, _) = newgit(&repo, &["env", "x"]);
    assert!(env.contains("minted"), "the capture read its input: {env}");
}

/// A failed action that declared captures: the terminal gets its stderr,
/// never the stdout its captures are read from, so a value that may be a
/// secret is not echoed because the command went on to fail. The log keeps
/// it. And `ran:` is the command as `--dry-run` renders it: an export's
/// value reaches the command through its environment, never the text.
#[test]
fn a_failed_capture_is_not_echoed_and_the_command_is_not_expanded() {
    let (_guard, temp) = tempdir();
    let repo = setup(
        &temp,
        r#"ownership = "branch"

[exports]
DB_PASSWORD = "hunter2-export"

[actions.mint]
command = "echo DB_URL=postgres://app:$(printf s3cret-%s capture)@db/app; echo \"pw=$DB_PASSWORD\"; echo mint-failed >&2; exit 6"
captures = ["DB_URL"]
"#,
    );
    let (code, _, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stderr}");

    let (code, stdout, stderr) = newgit(&repo, &["action", "db.mint", "x", "--quiet"]);
    assert_eq!(code, Some(6), "{stdout}{stderr}");
    let shown = format!("{stdout}{stderr}");
    assert!(!shown.contains("s3cret-capture"), "{shown}");
    assert!(!shown.contains("hunter2-export"), "{shown}");
    assert!(shown.contains("  | mint-failed"), "{shown}");
    assert!(shown.contains("stdout withheld"), "{shown}");
    let log = std::fs::read_to_string(action_log(&stderr)).expect("log");
    assert!(log.contains("s3cret-capture"), "the log keeps it: {log}");

    // Without `--quiet` the output never reached the terminal either, so
    // the failure is told the same way: the command and stderr's tail.
    let (code, stdout, loud) = newgit(&repo, &["action", "db.mint", "x"]);
    assert_eq!(code, Some(6), "{stdout}{loud}");
    let shown = format!("{stdout}{loud}");
    assert!(shown.contains("[newgit] ran: "), "{shown}");
    assert!(shown.contains("  | mint-failed"), "{shown}");
    assert!(!shown.contains("s3cret-capture"), "{shown}");

    let (_, plan, _) = newgit(&repo, &["action", "db.mint", "x", "--dry-run"]);
    let planned = plan
        .lines()
        .find_map(|line| line.strip_prefix("  command:  "))
        .expect("dry run names the command");
    let ran = stderr
        .lines()
        .find_map(|line| line.strip_prefix("[newgit] ran: "))
        .expect("ran:");
    assert_eq!(
        ran, planned,
        "the command that ran is the one --dry-run shows"
    );
    assert!(ran.contains("$DB_PASSWORD"), "{ran}");
}
