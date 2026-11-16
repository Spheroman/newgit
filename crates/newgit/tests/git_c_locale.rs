//! newgit runs its own Git in the C locale (`newgit_core::git::command()`),
//! so a developer's language never reaches the Git messages newgit matches.
//! That must not change the bytes newgit stores or shows: a non-ASCII file
//! name, commit message and author go through `capture`, `merge` and
//! `status`, run under a German locale, and come out as they went in.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

const NAME: &str = "résumé 日本.txt";
const NOTE: &str = "notes/café ü.txt";
const MESSAGE: &str = "Füße: 日本語の説明";
const AUTHOR: &str = "Zoë Ñúñez";

/// The developer's locale every newgit here runs under. Where Git has
/// German messages installed, they would be German.
const GERMAN: [(&str, &str); 3] = [
    ("LC_ALL", "de_DE.UTF-8"),
    ("LANG", "de_DE.UTF-8"),
    ("LANGUAGE", "de"),
];

fn git_output(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", AUTHOR)
        .env("GIT_AUTHOR_EMAIL", "zoe@example.com")
        .env("GIT_COMMITTER_NAME", AUTHOR)
        .env("GIT_COMMITTER_EMAIL", "zoe@example.com")
        .output()
        .expect("git runs")
}

fn git(dir: &Utf8Path, args: &[&str]) -> Vec<u8> {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
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
    let dir = std::env::temp_dir().join(format!("newgit-git-c-locale-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn newgit(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .envs(GERMAN)
        .env("GIT_AUTHOR_NAME", AUTHOR)
        .env("GIT_AUTHOR_EMAIL", "zoe@example.com")
        .env("GIT_COMMITTER_NAME", AUTHOR)
        .env("GIT_COMMITTER_EMAIL", "zoe@example.com")
        .output()
        .expect("newgit runs")
}

fn newgit_ok(dir: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(dir, args);
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn setup(temp: &Utf8Path) -> Utf8PathBuf {
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
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);
    newgit_ok(
        &repo,
        &[
            "tracker",
            "create",
            "notes",
            "--audience",
            "user",
            "--storage",
            "local",
        ],
    );
    newgit_ok(&repo, &["tracker", "track", "notes", "notes"]);
    repo
}

fn workspace(repo: &Utf8Path, instance: &str) -> Utf8PathBuf {
    newgit_ok(repo, &["spawn", instance]);
    Utf8PathBuf::from(newgit_ok(repo, &["status", instance, "--path"]).trim())
}

fn commit(workspace: &Utf8Path, contents: &str) {
    std::fs::write(workspace.join(NAME), contents).expect("write");
    git(workspace, &["add", "-A"]);
    git(workspace, &["commit", "-q", "-m", MESSAGE]);
}

/// NUL-separated names in `rev`'s tree, as bytes.
fn names(tracker_store: &Utf8Path, rev: &str) -> Vec<Vec<u8>> {
    git(
        tracker_store,
        &["ls-tree", "-r", "-z", "--name-only", "--full-tree", rev],
    )
    .split(|byte| *byte == 0)
    .filter(|name| !name.is_empty())
    .map(<[u8]>::to_vec)
    .collect()
}

#[test]
fn non_ascii_names_messages_and_authors_survive_capture_merge_and_status() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let tracker_store = repo.join(".newgit/local/trackers.git");
    let feature = workspace(&repo, "feature");

    commit(&feature, "feature\n");
    std::fs::create_dir_all(feature.join("notes")).expect("mkdir");
    std::fs::write(feature.join(NOTE), "n\n").expect("write");
    let captured = newgit_ok(&feature, &["tracker", "capture", "notes"]);
    assert!(captured.contains("1 file"), "{captured}");
    let status = newgit_ok(&repo, &["status", "feature"]);
    assert!(status.contains("ready"), "{status}");
    newgit_ok(&repo, &["merge", "feature"]);
    newgit_ok(&feature, &["tracker", "merge", "notes"]);

    // The names, exactly, in what newgit wrote.
    assert!(
        names(&tracker_store, "refs/trackers/root/main").contains(&NAME.as_bytes().to_vec()),
        "{:?}",
        names(&tracker_store, "refs/trackers/root/main")
    );
    assert!(
        names(&tracker_store, "refs/trackers/notes/main").contains(&NOTE.as_bytes().to_vec()),
        "{:?}",
        names(&tracker_store, "refs/trackers/notes/main")
    );

    // root's commit is newgit's split of the workspace's: its message and
    // author are the developer's, byte for byte. The capture commit is
    // newgit's own, by the author Git was told.
    let root = git(
        &tracker_store,
        &[
            "log",
            "-1",
            "--format=%an%x00%cn%x00%B",
            "refs/trackers/root/main",
            "--",
            NAME,
        ],
    );
    let root = String::from_utf8(root).expect("utf8 log");
    let mut fields = root.split('\0');
    assert_eq!(fields.next(), Some(AUTHOR));
    assert_eq!(fields.next(), Some(AUTHOR));
    assert!(fields.next().is_some_and(|body| body.starts_with(MESSAGE)));
    let notes = git(
        &tracker_store,
        &["log", "-1", "--format=%an", "refs/trackers/notes/main"],
    );
    assert_eq!(String::from_utf8(notes).expect("utf8").trim(), AUTHOR);
}

/// A conflict names the file as it is called, not as Git quotes it
/// (`"r\303\251sum\303\251 ..."`), and in English whatever the locale.
#[test]
fn a_conflict_on_a_non_ascii_file_names_it_as_it_is_called() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let first = workspace(&repo, "first");
    let second = workspace(&repo, "second");
    commit(&first, "first\n");
    commit(&second, "second\n");
    newgit_ok(&repo, &["merge", "first"]);
    let output = newgit(&repo, &["merge", "second"]);
    assert!(!output.status.success(), "a conflict");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains(NAME), "{stderr}");
    assert!(!stderr.contains("\\303"), "{stderr}");
}
