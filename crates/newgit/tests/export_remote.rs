//! `newgit export --remote <name>` (newgit-trackers.md, step 6): the
//! trackers' `main`s, composed with their history, pushed to an ordinary Git
//! repository defined in `.newgit/exports/<name>.toml`. A bare repository
//! stands in for GitHub.

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
    let dir = std::env::temp_dir().join(format!("newgit-export-{}-{n}", std::process::id()));
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

fn define(fixture: &Fixture, name: &str, contents: &str) {
    let dir = fixture.store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(format!("{name}.toml")), contents).expect("write");
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
fn nothing_public_exports_nothing_and_says_how_to_choose() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[]);
    define(
        &fixture,
        "github",
        &format!("url = \"{}\"\n", fixture.mirror),
    );

    let stderr = fails(&fixture.store, &["export", "--remote", "github"]);
    assert!(stderr.contains("trackers = [\"root\"]"), "{stderr}");
    assert_eq!(
        git(&fixture.mirror, &["for-each-ref"]),
        "",
        "nothing pushed"
    );
}

#[test]
fn named_trackers_ship_with_their_history_and_others_stay_out() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[]);
    define(
        &fixture,
        "github",
        &format!("url = \"{}\"\ntrackers = [\"root\"]\n", fixture.mirror),
    );

    let stdout = succeeds(&fixture.store, &["export", "--remote", "github"]);
    assert!(stdout.contains("withheld: project"), "{stdout}");
    let files = files_at(&fixture.mirror, "main");
    assert!(files.contains(&"README.md".to_owned()), "{files:?}");
    assert!(files.contains(&"src/lib.rs".to_owned()), "{files:?}");
    assert!(
        files.iter().all(|path| !path.starts_with(".newgit")),
        "`project` was not named, so `.newgit` stays out: {files:?}"
    );
    let subjects = git(&fixture.mirror, &["log", "--format=%s", "main"]);
    assert_eq!(subjects, "add lib\ninitial", "history, not one commit");
    assert_eq!(
        git(&fixture.mirror, &["rev-parse", "main"]),
        git(&fixture.store, &["rev-parse", "HEAD"]),
        "every tracker that touched these commits ships, so they are the originals"
    );
}

#[test]
fn exporting_again_fast_forwards_and_project_root_work_waits_for_merge() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[]);
    define(
        &fixture,
        "github",
        &format!("url = \"{}\"\ntrackers = [\"root\"]\n", fixture.mirror),
    );
    succeeds(&fixture.store, &["export", "--remote", "github"]);
    let first = git(&fixture.mirror, &["rev-parse", "main"]);

    std::fs::write(fixture.store.join("README.md"), "hello again\n").expect("write");
    git(&fixture.store, &["commit", "-q", "-am", "after init"]);
    let stdout = succeeds(&fixture.store, &["export", "--remote", "github"]);
    assert!(stdout.contains("newgit merge main"), "{stdout}");
    assert_eq!(
        git(&fixture.mirror, &["rev-parse", "main"]),
        first,
        "`main` has not moved, so neither has the export"
    );

    succeeds(&fixture.store, &["merge", "main"]);
    let stdout = succeeds(&fixture.store, &["export", "--remote", "github"]);
    assert!(!stdout.contains("newgit merge main"), "{stdout}");
    let second = git(&fixture.mirror, &["rev-parse", "main"]);
    assert_eq!(
        git(&fixture.mirror, &["log", "-1", "--format=%s", "main"]),
        "after init"
    );
    assert_eq!(git(&fixture.mirror, &["rev-parse", "main~1"]), first);
    assert_ne!(first, second);
}

#[test]
fn a_tracker_set_that_rewrites_history_needs_force() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[]);
    // `newgit init` left `.newgit` uncommitted; `merge main` publishes it.
    git(&fixture.store, &["add", ".newgit"]);
    git(&fixture.store, &["commit", "-q", "-m", "add newgit"]);
    std::fs::write(fixture.store.join("README.md"), "after\n").expect("write");
    git(&fixture.store, &["commit", "-q", "-am", "after newgit"]);
    succeeds(&fixture.store, &["merge", "main"]);
    define(
        &fixture,
        "github",
        &format!(
            "url = \"{}\"\ntrackers = [\"root\", \"project\", \"access\"]\n",
            fixture.mirror
        ),
    );
    succeeds(&fixture.store, &["export", "--remote", "github"]);
    let before = git(&fixture.mirror, &["rev-parse", "main"]);
    let files = files_at(&fixture.mirror, "main");
    assert!(
        files
            .iter()
            .any(|path| path.starts_with(".newgit/trackers/")),
        "{files:?}"
    );

    // Without `project` and `access`, the commit after theirs is rebuilt.
    define(
        &fixture,
        "github",
        &format!("url = \"{}\"\ntrackers = [\"root\"]\n", fixture.mirror),
    );
    let dry = succeeds(
        &fixture.store,
        &["export", "--remote", "github", "--dry-run"],
    );
    assert!(dry.contains("dry run"), "{dry}");
    assert_eq!(git(&fixture.mirror, &["rev-parse", "main"]), before);

    let stderr = fails(&fixture.store, &["export", "--remote", "github"]);
    assert!(stderr.contains("--force"), "{stderr}");
    assert_eq!(git(&fixture.mirror, &["rev-parse", "main"]), before);

    succeeds(&fixture.store, &["export", "--remote", "github", "--force"]);
    let files = files_at(&fixture.mirror, "main");
    assert!(
        files.iter().all(|path| !path.starts_with(".newgit")),
        "{files:?}"
    );
}

#[test]
fn per_target_views_differ() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[]);
    let private = temp.join("private.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", private.as_str()],
    );
    git(&fixture.store, &["add", ".newgit"]);
    git(&fixture.store, &["commit", "-q", "-m", "add newgit"]);
    succeeds(&fixture.store, &["merge", "main"]);
    define(
        &fixture,
        "public",
        &format!(
            "url = \"{}\"\nbranch = \"trunk\"\ntrackers = [\"root\"]\n",
            fixture.mirror
        ),
    );
    define(
        &fixture,
        "private",
        &format!("url = \"{private}\"\ntrackers = [\"root\", \"project\"]\n"),
    );
    succeeds(&fixture.store, &["export", "--remote", "public"]);
    succeeds(&fixture.store, &["export", "--remote", "private"]);

    let public = files_at(&fixture.mirror, "trunk");
    let private = files_at(&private, "main");
    assert!(public.iter().all(|path| !path.starts_with(".newgit")));
    assert!(private.iter().any(|path| path.starts_with(".newgit")));
}

#[test]
fn a_secret_in_the_history_refuses_the_export() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[(".env", "TOKEN=hunter2\n")]);
    succeeds(&fixture.store, &["tracker", "create", "runtime-env"]);
    succeeds(&fixture.store, &["tracker", "track", "runtime-env", ".env"]);
    define(
        &fixture,
        "github",
        &format!("url = \"{}\"\ntrackers = [\"root\"]\n", fixture.mirror),
    );

    let stderr = fails(&fixture.store, &["export", "--remote", "github"]);
    assert!(stderr.contains("runtime-env"), "{stderr}");
    assert_eq!(
        git(&fixture.mirror, &["for-each-ref"]),
        "",
        "nothing pushed"
    );
}

#[test]
fn a_secret_tracker_cannot_be_named() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[]);
    succeeds(&fixture.store, &["tracker", "create", "runtime-env"]);
    define(
        &fixture,
        "github",
        &format!(
            "url = \"{}\"\ntrackers = [\"root\", \"runtime-env\"]\n",
            fixture.mirror
        ),
    );
    let stderr = fails(&fixture.store, &["export", "--remote", "github"]);
    assert!(stderr.contains("is secret"), "{stderr}");

    define(
        &fixture,
        "github",
        &format!("url = \"{}\"\ntrackers = [\"nope\"]\n", fixture.mirror),
    );
    let stderr = fails(&fixture.store, &["export", "--remote", "github"]);
    assert!(stderr.contains("not a tracker"), "{stderr}");
}

#[test]
fn an_unknown_target_names_the_defined_ones() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp, &[]);
    define(&fixture, "github", "url = \"acme/widget\"\n");
    let stderr = fails(&fixture.store, &["export", "--remote", "gitlab"]);
    assert!(stderr.contains("defined: github"), "{stderr}");
}
