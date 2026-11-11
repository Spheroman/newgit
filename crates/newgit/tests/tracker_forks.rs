//! `newgit tracker create <name> --from <tracker>`: a fork, a tracker outside
//! Git whose history starts at another's, so the members of a role share
//! history. Its audience is a ceiling: never wider than its upstream's, at
//! creation, at every load of the definitions, and where a definition change
//! becomes a commit.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::TrackerDefinition;

const GITHUB_URL: &str = "https://github.com/acme/forks.git";

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

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-tracker-forks-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn newgit(dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

fn newgit_ok(dir: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(dir, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn newgit_err(dir: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(dir, args);
    assert!(!output.status.success(), "{args:?} should be refused");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A project pushing to a stand-in for GitHub, with `local-db` (outside Git,
/// claiming `.env.db`) holding one commit on its `main`.
fn setup(temp: &Utf8Path) -> Utf8PathBuf {
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

    newgit_ok(&repo, &["tracker", "create", "local-db"]);
    newgit_ok(&repo, &["tracker", "track", "local-db", ".env.db"]);
    std::fs::write(repo.join(".env.db"), "DATABASE_URL=postgres://local\n").expect("write");
    newgit_ok(&repo, &["tracker", "capture", "local-db", "--from-store"]);
    repo
}

fn tracker_ref(repo: &Utf8Path, reference: &str) -> String {
    git(
        repo,
        &[
            "--git-dir",
            repo.join(".newgit/local/trackers.git").as_str(),
            "rev-parse",
            reference,
        ],
    )
}

fn definition(repo: &Utf8Path, name: &str) -> TrackerDefinition {
    TrackerDefinition::from_file(name, &repo.join(format!(".newgit/trackers/{name}.toml")))
        .expect("definition")
}

#[test]
fn a_fork_starts_at_its_upstreams_history_and_fills_its_role() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);

    let out = newgit_ok(
        &repo,
        &[
            "tracker",
            "create",
            "staging-db",
            "--role",
            "db",
            "--from",
            "local-db",
        ],
    );
    assert!(out.contains("nothing is copied"), "{out}");
    assert!(out.contains("`local-db` now fills too"), "{out}");

    // One history: the fork's `main` is its upstream's very commit.
    assert_eq!(
        tracker_ref(&repo, "refs/trackers/staging-db/main"),
        tracker_ref(&repo, "refs/trackers/local-db/main"),
    );

    let fork = definition(&repo, "staging-db");
    assert_eq!(fork.from.as_deref(), Some("local-db"));
    assert_eq!(fork.role.as_deref(), Some("db"));
    assert_eq!(fork.audience, "project-devs");
    assert_eq!(fork.priority, -1);
    assert_eq!(fork.paths, vec![Utf8PathBuf::from(".env.db")]);
    assert!(fork.resources.is_empty());
    assert_eq!(definition(&repo, "local-db").role.as_deref(), Some("db"));

    // New instances keep binding the upstream; asked for, the fork comes
    // with the history it shares.
    newgit_ok(&repo, &["spawn", "feature-a", "--use", "staging-db"]);
    let path = newgit_ok(&repo, &["status", "feature-a", "--path"]);
    let workspace = Utf8PathBuf::from(path.trim());
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.db")).expect("bound"),
        "DATABASE_URL=postgres://local\n"
    );
}

#[test]
fn a_fork_cannot_be_wider_than_its_upstream() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    newgit_ok(
        &repo,
        &["tracker", "create", "notes", "--audience", "user:jack"],
    );
    newgit_ok(&repo, &["tracker", "track", "notes", "notes.md"]);
    std::fs::write(repo.join("notes.md"), "private\n").expect("write");
    newgit_ok(&repo, &["tracker", "capture", "notes", "--from-store"]);

    let err = newgit_err(
        &repo,
        &[
            "tracker",
            "create",
            "shared-notes",
            "--from",
            "notes",
            "--role",
            "notes",
            "--audience",
            "project-devs",
        ],
    );
    assert!(
        err.contains("`project-devs`") && err.contains("`user:jack`"),
        "{err}"
    );
    assert!(!repo.join(".newgit/trackers/shared-notes.toml").exists());

    // Narrower is fine: a fork of a shared tracker that only jack reads.
    newgit_ok(
        &repo,
        &[
            "tracker",
            "create",
            "my-db",
            "--from",
            "local-db",
            "--role",
            "db",
            "--audience",
            "user:jack",
        ],
    );
}

#[test]
fn widening_a_fork_later_is_refused_on_load() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    newgit_ok(
        &repo,
        &[
            "tracker",
            "create",
            "staging-db",
            "--role",
            "db",
            "--from",
            "local-db",
        ],
    );
    let file = repo.join(".newgit/trackers/staging-db.toml");
    let text = std::fs::read_to_string(&file).expect("read");
    std::fs::write(
        &file,
        text.replace("audience = \"project-devs\"", "audience = \"public\""),
    )
    .expect("write");
    let err = newgit_err(&repo, &["tracker", "list"]);
    assert!(
        err.contains("staging-db") && err.contains("wider than `local-db`'s"),
        "{err}"
    );
}

#[test]
fn widening_a_fork_in_a_commit_is_refused_where_it_leaves() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    newgit_ok(
        &repo,
        &[
            "tracker",
            "create",
            "staging-db",
            "--role",
            "db",
            "--from",
            "local-db",
        ],
    );
    git(&repo, &["add", ".newgit/trackers", ".gitignore"]);
    git(&repo, &["commit", "-q", "-m", "Add staging-db"]);
    git(&repo, &["push", "-q", "origin", "main"]);
    newgit_ok(&repo, &["spawn", "feature-a"]);
    let path = newgit_ok(&repo, &["status", "feature-a", "--path"]);
    let workspace = Utf8PathBuf::from(path.trim());

    let file = workspace.join(".newgit/trackers/staging-db.toml");
    let text = std::fs::read_to_string(&file).expect("read");
    std::fs::write(
        &file,
        text.replace("audience = \"project-devs\"", "audience = \"public\""),
    )
    .expect("write");
    git(&workspace, &["add", ".newgit/trackers/staging-db.toml"]);
    git(&workspace, &["commit", "-q", "-m", "Publish staging-db"]);
    let output = git_output(&workspace, &["push", "-q", "origin", "feature-a"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the push should be refused");
    assert!(
        stderr.contains("changes tracker definitions") && stderr.contains("wider than"),
        "{stderr}"
    );
}

#[test]
fn what_a_fork_needs_is_asked_for() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);

    // Without a role on either side, the two would claim one path.
    let err = newgit_err(
        &repo,
        &["tracker", "create", "staging-db", "--from", "local-db"],
    );
    assert!(err.contains("--role <role>"), "{err}");

    newgit_ok(
        &repo,
        &[
            "tracker",
            "create",
            "staging-db",
            "--role",
            "db",
            "--from",
            "local-db",
        ],
    );
    // A fork fills its upstream's role.
    let err = newgit_err(
        &repo,
        &[
            "tracker", "create", "other-db", "--role", "cache", "--from", "local-db",
        ],
    );
    assert!(err.contains("fills role `db`"), "{err}");

    // An upstream with no history has nothing to share.
    newgit_ok(&repo, &["tracker", "create", "empty"]);
    let err = newgit_err(
        &repo,
        &[
            "tracker", "create", "empty-2", "--role", "e", "--from", "empty",
        ],
    );
    assert!(err.contains("has no history yet"), "{err}");

    // The upstream is the ceiling, so it stays while a fork names it.
    let err = newgit_err(&repo, &["tracker", "remove", "local-db"]);
    assert!(
        err.contains("`staging-db` is a fork of `local-db`"),
        "{err}"
    );

    // A fork claims what its upstream claims.
    let err = newgit_err(
        &repo,
        &[
            "tracker", "create", "third-db", "--from", "local-db", "--path", "x",
        ],
    );
    assert!(err.contains("leave out --path"), "{err}");
}
