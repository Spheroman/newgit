//! Where a Claude Code worktree's instance starts: the project branch as it
//! stands — its upstream when the project root's branch is only behind it —
//! fetched at most once for subagents launched together, so they all start
//! from the same commit. Not the project root's checked-out `HEAD`, which
//! is what a plain `newgit spawn` uses.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
];

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .envs(IDENTITY)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit(dir: &Utf8Path, file: &str) -> String {
    std::fs::write(dir.join(file), format!("{file}\n")).expect("write");
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", file]);
    git(dir, &["rev-parse", "HEAD"])
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
    let dir = std::env::temp_dir().join(format!("newgit-cc-base-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

/// A project root whose `main` tracks `origin/main` on a bare repo, and a
/// second clone of that repo standing in for everyone else (a PR merged on
/// GitHub). Returns (root, other).
fn setup(temp: &Utf8Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let upstream = temp.join("upstream.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", upstream.as_str()],
    );

    let root = temp.join("store");
    std::fs::create_dir_all(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    commit(&root, "README.md");
    git(&root, &["remote", "add", "origin", upstream.as_str()]);
    git(&root, &["push", "-q", "-u", "origin", "main"]);

    let store = MetadataStore::init(&root, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");

    let other = temp.join("other");
    git(temp, &["clone", "-q", upstream.as_str(), other.as_str()]);
    std::fs::create_dir_all(temp.join("home")).expect("mkdir home");
    (root, other)
}

/// Forget that the root ever fetched, so the next hook fetches.
fn forget_fetch(root: &Utf8Path) {
    let fetch_head = git(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "FETCH_HEAD",
        ],
    );
    let _ = std::fs::remove_file(fetch_head);
}

fn create(temp: &Utf8Path, root: &Utf8Path, name: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(root.as_str())
        .args(["claude-code", "worktree-create"])
        // Not the developer's own `~/.claude/settings.json`.
        .env("HOME", temp.join("home").as_str())
        .envs(IDENTITY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("newgit runs");
    let input = serde_json::json!({ "cwd": root, "name": name }).to_string();
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("newgit exits");
    assert!(output.status.success(), "{name}: {output:?}");
    output
}

fn source_rev(root: &Utf8Path, name: &str) -> String {
    MetadataStore::at(root)
        .find_branch(name)
        .expect("instance record")
        .source_rev
}

/// The bug: the root's `main` was 20 commits behind `origin/main`, and every
/// subagent's instance started on the stale `main` — then each agent reset
/// itself onto GitHub's by hand, which `status` showed as different bases.
#[test]
fn starts_from_the_upstream_when_the_root_branch_is_behind() {
    let (_guard, temp) = tempdir();
    let (root, other) = setup(&temp);
    let stale = git(&root, &["rev-parse", "main"]);
    let merged = commit(&other, "merged-on-github");
    git(&other, &["push", "-q", "origin", "main"]);
    git(&root, &["fetch", "-q", "origin"]);

    // Pushed after the root's fetch: within the fetch window, the hook uses
    // the ref the root already has rather than fetching again.
    commit(&other, "pushed-after-the-fetch");
    git(&other, &["push", "-q", "origin", "main"]);

    create(&temp, &root, "agent-one");
    assert_eq!(source_rev(&root, "agent-one"), merged);
    assert_eq!(
        git(&root, &["rev-parse", "main"]),
        stale,
        "the root's own branch is left alone"
    );
}

/// Subagents launched together run their hooks at the same moment, against
/// a root that has not fetched in a day while the upstream has moved. One of
/// them fetches; all of them start from what it fetched.
#[test]
fn concurrent_hooks_agree_on_one_fetched_base() {
    let (_guard, temp) = tempdir();
    let (root, other) = setup(&temp);
    let newest = commit(&other, "merged-on-github");
    git(&other, &["push", "-q", "origin", "main"]);
    forget_fetch(&root);

    let names: Vec<String> = (0..6).map(|n| format!("agent-{n}")).collect();
    std::thread::scope(|scope| {
        for name in &names {
            let (temp, root) = (&temp, &root);
            scope.spawn(move || create(temp, root, name));
        }
    });

    for name in &names {
        assert_eq!(source_rev(&root, name), newest, "{name}");
    }
    assert_eq!(git(&root, &["rev-parse", "origin/main"]), newest);
}

/// Work the root has that its upstream does not yet — a `newgit merge` not
/// exported, say — is the newer line, and the base.
#[test]
fn starts_from_the_root_branch_when_it_is_ahead() {
    let (_guard, temp) = tempdir();
    let (root, _other) = setup(&temp);
    let ahead = commit(&root, "merged-here");
    git(&root, &["fetch", "-q", "origin"]);

    create(&temp, &root, "agent-one");
    let record = MetadataStore::at(&root)
        .find_branch("agent-one")
        .expect("record");
    assert_eq!(record.source_rev, ahead);
    assert_eq!(
        record.base_ref.as_deref(),
        Some("main"),
        "drift is reported against it"
    );
}

/// The root's checkout is someone's topic branch: the default still starts
/// from the project branch, and Claude Code's `worktree.baseRef = "head"`
/// starts from the checkout instead, as Claude Code's own worktrees would.
#[test]
fn base_ref_head_starts_from_the_checkout() {
    let (_guard, temp) = tempdir();
    let (root, _other) = setup(&temp);
    let main = git(&root, &["rev-parse", "main"]);
    git(&root, &["switch", "-q", "-c", "topic"]);
    let topic = commit(&root, "topic-work");
    git(&root, &["fetch", "-q", "origin"]);

    create(&temp, &root, "fresh");
    assert_eq!(source_rev(&root, "fresh"), main);

    std::fs::create_dir_all(root.join(".claude")).expect("mkdir");
    std::fs::write(
        root.join(".claude/settings.local.json"),
        r#"{"worktree":{"baseRef":"head"}}"#,
    )
    .expect("write settings");
    create(&temp, &root, "head");
    assert_eq!(source_rev(&root, "head"), topic);
}

/// The upstream fails (newgit.dev answering 500, say) while subagents
/// launch. None of them fails to spawn, and none retries the fetch and
/// starts somewhere the others did not, even once the upstream is back.
#[test]
fn a_failed_fetch_falls_back_to_the_ref_here_for_the_whole_burst() {
    let (_guard, temp) = tempdir();
    let (root, other) = setup(&temp);
    git(&root, &["fetch", "-q", "origin"]);
    let cached = git(&root, &["rev-parse", "origin/main"]);
    commit(&other, "merged-on-github");
    git(&other, &["push", "-q", "origin", "main"]);
    forget_fetch(&root);
    let upstream = temp.join("upstream.git");
    let away = temp.join("upstream-away.git");
    std::fs::rename(&upstream, &away).expect("take the upstream away");

    let names: Vec<String> = (0..3).map(|n| format!("agent-{n}")).collect();
    std::thread::scope(|scope| {
        for name in &names {
            let (temp, root) = (&temp, &root);
            scope.spawn(move || create(temp, root, name));
        }
    });
    std::fs::rename(&away, &upstream).expect("bring the upstream back");
    create(&temp, &root, "agent-late");

    for name in names.iter().map(String::as_str).chain(["agent-late"]) {
        assert_eq!(source_rev(&root, name), cached, "{name}");
    }
}
