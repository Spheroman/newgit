//! #96: a workspace's `origin` behaves like the project's real remote. It
//! fetches from the store's `origin` directly, and a plain `git push` goes
//! to newgit's outbox, which checkpoints the instance and forwards the push,
//! succeeding exactly when the real remote accepted it.
//!
//! The "real remote" here is a bare repository standing in for GitHub: the
//! store's `origin` is a GitHub URL, rewritten to the bare repository with
//! `url.<path>.insteadOf` in the store's config — so what the workspace is
//! given is the URL Git would really use, the bare repository's path.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

const GITHUB_URL: &str = "https://github.com/acme/widget.git";

fn git_output(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs")
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops — the same hand-rolled guard as `spawn_exit.rs`.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-push-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

struct Fixture {
    store: Utf8PathBuf,
    remote: Utf8PathBuf,
}

fn setup(temp: &Utf8Path) -> Fixture {
    let remote = temp.join("github.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", remote.as_str()],
    );

    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    git(&repo, &["remote", "add", "origin", GITHUB_URL]);
    git(
        &repo,
        &["config", &format!("url.{remote}.insteadOf"), GITHUB_URL],
    );
    git(&repo, &["push", "-q", "origin", "main"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    Fixture {
        store: repo,
        remote,
    }
}

fn newgit(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

fn spawn(fixture: &Fixture, name: &str) -> (Utf8PathBuf, String) {
    let output = newgit(&fixture.store, &["spawn", name]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "spawn failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = newgit(&fixture.store, &["status", name, "--path"]);
    let workspace = Utf8PathBuf::from(String::from_utf8_lossy(&path.stdout).trim());
    (workspace, stdout)
}

fn commit(workspace: &Utf8Path, file: &str, contents: &str) -> String {
    std::fs::write(workspace.join(file), contents).expect("write");
    git(workspace, &["add", file]);
    git(workspace, &["commit", "-q", "-m", file]);
    git(workspace, &["rev-parse", "HEAD"])
}

fn rev(repo: &Utf8Path, name: &str) -> Option<String> {
    let output = git_output(repo, &["rev-parse", "--verify", "--quiet", name]);
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn checkpoints(fixture: &Fixture, name: &str) -> String {
    let output = newgit(&fixture.store, &["checkpoints", name]);
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn git_push_checkpoints_then_publishes_to_the_real_remote() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, spawn_output) = spawn(&fixture, "feature/push");
    assert!(
        spawn_output.contains(&format!(
            "`git push` checkpoints, then publishes to {}",
            fixture.remote
        )),
        "{spawn_output}"
    );

    let tip = commit(&workspace, "a.txt", "a\n");
    // Uncommitted work too: the checkpoint protects the worktree, not just
    // what was pushed.
    std::fs::write(workspace.join("scratch.txt"), "wip\n").expect("write");

    let push = git_output(&workspace, &["push", "origin", "feature/push"]);
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(push.status.success(), "push failed:\n{stderr}");
    assert!(stderr.contains("published to origin"), "{stderr}");

    assert_eq!(
        rev(&fixture.remote, "refs/heads/feature/push"),
        Some(tip.clone())
    );
    assert_eq!(rev(&fixture.store, "refs/heads/feature/push"), Some(tip));

    let listing = checkpoints(&fixture, "feature/push");
    assert!(listing.contains("push"), "{listing}");
    assert!(listing.contains("before `git push` to origin"), "{listing}");
}

#[test]
fn a_rejection_from_the_real_remote_refuses_the_push() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/rejected");
    let base = rev(&fixture.store, "refs/heads/feature/rejected");

    let hook = fixture.remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\necho 'protected branch'\nexit 1\n").expect("write hook");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    commit(&workspace, "a.txt", "a\n");
    let push = git_output(&workspace, &["push", "origin", "feature/rejected"]);
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(!push.status.success(), "push should fail:\n{stderr}");
    assert!(stderr.contains("protected branch"), "{stderr}");

    assert_eq!(rev(&fixture.remote, "refs/heads/feature/rejected"), None);
    // The checkpoint still happened — it comes first, and is harmless.
    assert_ne!(rev(&fixture.store, "refs/heads/feature/rejected"), base);
}

#[test]
fn a_store_without_origin_refuses_the_push_instead_of_keeping_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    git(&fixture.store, &["remote", "remove", "origin"]);
    let (workspace, spawn_output) = spawn(&fixture, "feature/nowhere");
    assert!(spawn_output.contains("will be refused"), "{spawn_output}");
    let base = rev(&fixture.store, "refs/heads/feature/nowhere");

    commit(&workspace, "a.txt", "a\n");
    let push = git_output(&workspace, &["push", "origin", "feature/nowhere"]);
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(!push.status.success(), "push should fail:\n{stderr}");
    assert!(stderr.contains("has no `origin` remote"), "{stderr}");
    assert_eq!(rev(&fixture.store, "refs/heads/feature/nowhere"), base);
}

#[test]
fn a_force_push_is_forwarded_as_a_force_push() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/force");

    commit(&workspace, "a.txt", "a\n");
    let first = git_output(&workspace, &["push", "origin", "feature/force"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    git(&workspace, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let rewritten = git(&workspace, &["rev-parse", "HEAD"]);
    let forced = git_output(&workspace, &["push", "--force", "origin", "feature/force"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert_eq!(
        rev(&fixture.remote, "refs/heads/feature/force"),
        Some(rewritten)
    );
}

#[test]
fn other_branches_and_tags_are_published_too() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/extra");
    let tip = commit(&workspace, "a.txt", "a\n");
    git(&workspace, &["tag", "v1"]);

    let push = git_output(
        &workspace,
        &["push", "origin", "HEAD:refs/heads/other", "v1"],
    );
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert_eq!(rev(&fixture.remote, "refs/heads/other"), Some(tip.clone()));
    assert_eq!(rev(&fixture.remote, "refs/tags/v1^{commit}"), Some(tip));
}

#[test]
fn deleting_the_instance_branch_from_its_workspace_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/keep");
    let first = git_output(&workspace, &["push", "origin", "feature/keep"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let push = git_output(&workspace, &["push", "origin", "--delete", "feature/keep"]);
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(
        !push.status.success(),
        "delete should be refused:\n{stderr}"
    );
    assert!(stderr.contains("newgit remove feature/keep"), "{stderr}");
    assert!(rev(&fixture.store, "refs/heads/feature/keep").is_some());
    assert!(rev(&fixture.remote, "refs/heads/feature/keep").is_some());
}

#[test]
fn origin_reads_as_the_real_remote_so_gh_can_resolve_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/gh");

    // `gh` reads `git remote -v` and takes the fetch URL; it must name the
    // real repository, not a local path.
    let remotes = git(&workspace, &["remote", "-v"]);
    assert!(
        remotes.contains(&format!("origin\t{} (fetch)", fixture.remote)),
        "{remotes}"
    );
}

#[test]
fn git_pull_reads_the_real_remote() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/pull");
    commit(&workspace, "a.txt", "a\n");
    let push = git_output(&workspace, &["push", "-u", "origin", "feature/pull"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );

    // Someone else pushes to the real remote — a reviewer's suggestion.
    let other = temp.join("other");
    git(
        &temp,
        &[
            "clone",
            "-q",
            "--branch",
            "feature/pull",
            fixture.remote.as_str(),
            other.as_str(),
        ],
    );
    let theirs = commit(&other, "b.txt", "b\n");
    git(&other, &["push", "-q", "origin", "feature/pull"]);

    // A push without their commit is refused, as the real remote would...
    commit(&workspace, "c.txt", "c\n");
    let stale = git_output(&workspace, &["push", "origin", "feature/pull"]);
    assert!(
        !stale.status.success(),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert_ne!(rev(&fixture.remote, "refs/heads/feature/pull"), None);

    // ...and `git pull` gets it, as it would from any clone.
    git(&workspace, &["pull", "-q", "--no-rebase", "--no-edit"]);
    let merged = git_output(
        &workspace,
        &["merge-base", "--is-ancestor", &theirs, "HEAD"],
    );
    assert!(merged.status.success());
    let push = git_output(&workspace, &["push", "origin", "feature/pull"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
}

#[test]
fn force_with_lease_holds_across_a_checkpoint() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/lease");
    commit(&workspace, "a.txt", "a\n");
    let push = git_output(&workspace, &["push", "-u", "origin", "feature/lease"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );

    // A checkpoint moves the store's branch. The lease is about the real
    // remote's, which that must not disturb.
    commit(&workspace, "b.txt", "b\n");
    let checkpoint = newgit(&workspace, &["checkpoint"]);
    assert!(
        checkpoint.status.success(),
        "{}",
        String::from_utf8_lossy(&checkpoint.stderr)
    );

    git(&workspace, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let rewritten = git(&workspace, &["rev-parse", "HEAD"]);
    let leased = git_output(
        &workspace,
        &["push", "--force-with-lease", "origin", "feature/lease"],
    );
    assert!(
        leased.status.success(),
        "{}",
        String::from_utf8_lossy(&leased.stderr)
    );
    assert_eq!(
        rev(&fixture.remote, "refs/heads/feature/lease"),
        Some(rewritten)
    );
}

#[test]
fn a_workspace_that_predates_the_push_route_gets_it_at_its_next_checkpoint() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (workspace, _) = spawn(&fixture, "feature/old");
    // What `spawn` did before the route existed: `origin` is the store.
    git(
        &workspace,
        &["config", "--unset", "remote.origin.receivepack"],
    );
    git(&workspace, &["config", "--unset", "remote.origin.pushurl"]);
    git(
        &workspace,
        &["remote", "set-url", "origin", fixture.store.as_str()],
    );

    let checkpoint = newgit(&workspace, &["checkpoint"]);
    assert!(
        checkpoint.status.success(),
        "{}",
        String::from_utf8_lossy(&checkpoint.stderr)
    );
    let tip = commit(&workspace, "a.txt", "a\n");
    let push = git_output(&workspace, &["push", "origin", "feature/old"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert_eq!(rev(&fixture.remote, "refs/heads/feature/old"), Some(tip));
}
