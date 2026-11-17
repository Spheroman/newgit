//! An export checks only what it would newly publish (#444): the commits its
//! target does not have yet, asked of the target as it is now. One old
//! commit that added a file before a tracker outside Git claimed it no
//! longer blocks every export; a new one still does, and says the way out;
//! and a target force-pushed to drop that commit is not trusted to have it.
//! A bare repository stands in for GitHub.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops — the same hand-rolled guard as `push.rs`.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-export-new-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

struct Fixture {
    store: Utf8PathBuf,
    mirror: Utf8PathBuf,
}

/// A project with two commits of its own, initialized, and an empty bare
/// repository to export to. `files` go into the first commit too.
fn setup(temp: &Utf8Path, files: &[(&str, &str)]) -> Fixture {
    let mirror = temp.join("mirror.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );

    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    for (path, contents) in files {
        std::fs::write(repo.join(path), contents).expect("write");
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    std::fs::write(repo.join("src/lib.rs"), "fn main() {}\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "add lib"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    Fixture {
        store: repo,
        mirror,
    }
}

/// Set tracker `tracker`'s `export` to `export` as written, or take it out
/// (uncommitted: definitions are read from the working tree).
fn set_export(fixture: &Fixture, tracker: &str, export: Option<&str>) {
    let path = fixture
        .store
        .join(format!(".newgit/trackers/{tracker}.toml"));
    let text = std::fs::read_to_string(&path).expect("definition");
    let mut kept: String = text
        .lines()
        .filter(|line| !line.starts_with("export "))
        .map(|line| format!("{line}\n"))
        .collect();
    if let Some(export) = export {
        kept.push_str(&format!("export = {export}\n"));
    }
    std::fs::write(&path, kept).expect("write");
}

/// Root declares target `name` at `url`; `access` inherits it when `access`.
fn define(fixture: &Fixture, name: &str, url: &Utf8Path, access: bool) {
    set_export(
        fixture,
        "root",
        Some(&format!("{{ url = \"{url}\", name = \"{name}\" }}")),
    );
    set_export(fixture, "access", access.then_some("\"inherit\""));
}

fn newgit(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

fn succeeds(dir: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

fn fails(dir: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(dir, args);
    assert!(
        !output.status.success(),
        "newgit {args:?} should have failed:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn files_at(repo: &Utf8Path, rev: &str) -> Vec<String> {
    git(repo, &["ls-tree", "-r", "--name-only", rev])
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

#[test]
fn an_old_commit_the_target_has_does_not_block_the_export() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[(".env", "TOKEN=old-and-published\n")]);
    define(&fixture, "github", &fixture.mirror, false);
    succeeds(&fixture.store, &["export", "--remote", "github"]);

    // `.env` becomes a tracker outside Git's after it was published.
    succeeds(&fixture.store, &["tracker", "create", "runtime-env"]);
    succeeds(&fixture.store, &["tracker", "track", "runtime-env", ".env"]);
    git(&fixture.store, &["rm", "-q", "--cached", ".env"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", ".env leaves Git"]);
    std::fs::write(fixture.store.join("src/more.rs"), "fn more() {}\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "more"]);
    succeeds(&fixture.store, &["merge", "main"]);

    succeeds(&fixture.store, &["export", "--remote", "github"]);
    let exported = files_at(&fixture.mirror, "main");
    assert!(exported.contains(&"src/more.rs".to_owned()), "{exported:?}");
    assert!(!exported.contains(&".env".to_owned()), "{exported:?}");
}

#[test]
fn a_new_commit_adding_a_file_outside_git_is_refused_and_says_the_way_out() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[]);
    define(&fixture, "github", &fixture.mirror, false);
    succeeds(&fixture.store, &["export", "--remote", "github"]);
    let before = git(&fixture.mirror, &["rev-parse", "main"]);

    // `.env` committed, and merged, before anything claimed it; never
    // exported.
    std::fs::write(fixture.store.join(".env"), "TOKEN=never-published\n").expect("write");
    git(&fixture.store, &["add", ".env"]);
    git(&fixture.store, &["commit", "-q", "-m", "oops"]);
    succeeds(&fixture.store, &["merge", "main"]);
    succeeds(&fixture.store, &["tracker", "create", "runtime-env"]);
    succeeds(&fixture.store, &["tracker", "track", "runtime-env", ".env"]);
    git(&fixture.store, &["rm", "-q", "--cached", ".env"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", ".env leaves Git"]);
    succeeds(&fixture.store, &["merge", "main"]);

    // Deleting it did not help: the commit adding it is new to the target.
    let stderr = fails(&fixture.store, &["export", "--remote", "github"]);
    assert!(
        stderr.contains("`.env` belongs to `runtime-env`"),
        "{stderr}"
    );
    assert!(stderr.contains("does not have that commit"), "{stderr}");
    assert!(stderr.contains("not checked again"), "{stderr}");
    assert_eq!(git(&fixture.mirror, &["rev-parse", "main"]), before);
    let stderr = fails(
        &fixture.store,
        &["export", "--remote", "github", "--dry-run"],
    );
    assert!(stderr.contains("does not have that commit"), "{stderr}");
}

#[test]
fn a_target_force_pushed_without_the_commit_is_asked_not_remembered() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[(".env", "TOKEN=purged-there\n")]);
    define(&fixture, "github", &fixture.mirror, false);
    succeeds(&fixture.store, &["export", "--remote", "github"]);
    succeeds(&fixture.store, &["tracker", "create", "runtime-env"]);
    succeeds(&fixture.store, &["tracker", "track", "runtime-env", ".env"]);
    git(&fixture.store, &["rm", "-q", "--cached", ".env"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", ".env leaves Git"]);

    // The target purges it: a history of its own, force-pushed.
    let purge = temp.join("purge");
    git(&temp, &["init", "-q", "-b", "main", purge.as_str()]);
    std::fs::write(purge.join("README.md"), "hello\n").expect("write");
    git(&purge, &["add", "."]);
    git(&purge, &["commit", "-q", "-m", "purged"]);
    git(
        &purge,
        &[
            "push",
            "-q",
            "--force",
            fixture.mirror.as_str(),
            "main:main",
        ],
    );
    let purged = git(&fixture.mirror, &["rev-parse", "main"]);

    // What this machine pushed before is not what the target has now.
    let stderr = fails(&fixture.store, &["export", "--remote", "github", "--force"]);
    assert!(stderr.contains("does not have that commit"), "{stderr}");
    assert_eq!(git(&fixture.mirror, &["rev-parse", "main"]), purged);
}
