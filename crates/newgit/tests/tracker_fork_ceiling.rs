//! The ceiling on a fork holds after its definition changes (#415 review):
//! dropping or changing `from` does not lift it, at edit time or where a
//! commit leaves; a merge that breaks a fork is refused; a broken fork can
//! still be listed and removed; and `--from` says what it names.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

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
    let dir = std::env::temp_dir().join(format!("newgit-fork-ceiling-{}-{n}", std::process::id()));
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

/// `staging-db`, a fork of `local-db` filling role `db`, committed and
/// pushed, and an instance `feature-a` spawned after it.
fn with_fork(repo: &Utf8Path) -> Utf8PathBuf {
    newgit_ok(
        repo,
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
    git(repo, &["add", ".newgit/trackers", ".gitignore"]);
    git(repo, &["commit", "-q", "-m", "Add staging-db"]);
    git(repo, &["push", "-q", "origin", "main"]);
    newgit_ok(repo, &["spawn", "feature-a"]);
    Utf8PathBuf::from(newgit_ok(repo, &["status", "feature-a", "--path"]).trim())
}

fn edit(file: &Utf8Path, edit: impl Fn(String) -> String) {
    let text = std::fs::read_to_string(file).expect("read");
    std::fs::write(file, edit(text)).expect("write");
}

fn drop_from_and_publish(text: String) -> String {
    text.replace("from = \"local-db\"\n", "")
        .replace("audience = \"project-devs\"", "audience = \"public\"")
}

fn push_refused(workspace: &Utf8Path) -> String {
    let output = git_output(workspace, &["push", "-q", "origin", "feature-a"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "the push should be refused");
    stderr
}

#[test]
fn dropping_from_does_not_lift_the_ceiling_at_edit_time() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    with_fork(&repo);
    edit(
        &repo.join(".newgit/trackers/staging-db.toml"),
        drop_from_and_publish,
    );
    assert!(
        !std::fs::read_to_string(repo.join(".newgit/trackers/staging-db.toml"))
            .expect("read")
            .contains("from =")
    );
    let err = newgit_err(&repo, &["status"]);
    assert!(
        err.contains("was a fork of `local-db`")
            && err.contains("no longer says `from`")
            && err.contains("`project-devs`"),
        "{err}"
    );
}

#[test]
fn dropping_from_in_a_commit_is_refused_where_it_leaves() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let workspace = with_fork(&repo);
    edit(
        &workspace.join(".newgit/trackers/staging-db.toml"),
        drop_from_and_publish,
    );
    git(
        &workspace,
        &["commit", "-q", "-am", "Detach and publish staging-db"],
    );
    let stderr = push_refused(&workspace);
    assert!(
        stderr.contains("changes tracker definitions") && stderr.contains("no longer says `from`"),
        "{stderr}"
    );
}

#[test]
fn dropping_from_then_widening_in_two_commits_is_refused() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let workspace = with_fork(&repo);
    let file = workspace.join(".newgit/trackers/staging-db.toml");
    // Still within `local-db`'s audience, so the first alone is fine.
    edit(&file, |text| text.replace("from = \"local-db\"\n", ""));
    git(&workspace, &["commit", "-q", "-am", "Detach staging-db"]);
    edit(&file, |text| {
        text.replace("audience = \"project-devs\"", "audience = \"public\"")
    });
    git(&workspace, &["commit", "-q", "-am", "Publish staging-db"]);
    let stderr = push_refused(&workspace);
    assert!(
        stderr.contains("was a fork of `local-db`") && stderr.contains("`public`"),
        "{stderr}"
    );
}

#[test]
fn repointing_from_does_not_lift_the_ceiling() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    newgit_ok(
        &repo,
        &[
            "tracker",
            "create",
            "public-db",
            "--audience",
            "public",
            "--role",
            "db",
        ],
    );
    let workspace = with_fork(&repo);
    // A public upstream, which `staging-db`'s history never came from.
    edit(
        &workspace.join(".newgit/trackers/staging-db.toml"),
        |text| {
            text.replace("from = \"local-db\"", "from = \"public-db\"")
                .replace("audience = \"project-devs\"", "audience = \"public\"")
        },
    );
    git(&workspace, &["commit", "-q", "-am", "Repoint staging-db"]);
    let stderr = push_refused(&workspace);
    assert!(
        stderr.contains("now says `from = \"public-db\"`") && stderr.contains("`local-db`"),
        "{stderr}"
    );

    // And in the project, at edit time.
    edit(&repo.join(".newgit/trackers/staging-db.toml"), |text| {
        text.replace("from = \"local-db\"", "from = \"public-db\"")
            .replace("audience = \"project-devs\"", "audience = \"public\"")
    });
    let err = newgit_err(&repo, &["status"]);
    assert!(err.contains("was a fork of `local-db`"), "{err}");
}

#[test]
fn merging_a_fork_with_its_upstreams_narrowing_is_refused() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    // `local-db` fills `db` (making the fork wrote it), and the fork's
    // definition is kept for a branch to add.
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
    let fork =
        std::fs::read_to_string(repo.join(".newgit/trackers/staging-db.toml")).expect("read");
    newgit_ok(&repo, &["tracker", "remove", "staging-db"]);
    git(&repo, &["add", "-A", ".newgit/trackers", ".gitignore"]);
    git(&repo, &["commit", "-q", "-m", "local-db fills db"]);
    git(&repo, &["push", "-q", "origin", "main"]);
    newgit_ok(&repo, &["spawn", "feature-a"]);
    let workspace = Utf8PathBuf::from(newgit_ok(&repo, &["status", "feature-a", "--path"]).trim());

    // One side adds the fork, the other narrows its upstream: each is fine.
    git(&workspace, &["switch", "-q", "-c", "side"]);
    std::fs::write(workspace.join(".newgit/trackers/staging-db.toml"), fork).expect("write");
    git(&workspace, &["add", ".newgit/trackers/staging-db.toml"]);
    git(&workspace, &["commit", "-q", "-m", "Add staging-db"]);
    git(&workspace, &["switch", "-q", "feature-a"]);
    edit(&workspace.join(".newgit/trackers/local-db.toml"), |text| {
        text.replace("audience = \"project-devs\"", "audience = \"user:jack\"")
    });
    git(&workspace, &["commit", "-q", "-am", "Narrow local-db"]);
    git(&workspace, &["merge", "-q", "--no-edit", "side"]);

    let stderr = push_refused(&workspace);
    assert!(
        stderr.contains("changes tracker definitions")
            && stderr.contains("wider than `local-db`'s `user:jack`"),
        "{stderr}"
    );
}

#[test]
fn a_broken_fork_can_still_be_listed_and_removed() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    with_fork(&repo);
    // Its upstream narrowed below it, by hand.
    let upstream = repo.join(".newgit/trackers/local-db.toml");
    edit(&upstream, |text| {
        text.replace("audience = \"project-devs\"", "audience = \"user:jack\"")
    });
    let err = newgit_err(&repo, &["status"]);
    assert!(
        err.contains("`newgit tracker remove staging-db`")
            && err.contains("audience = \"user:jack\""),
        "{err}"
    );
    newgit_err(&repo, &["spawn", "feature-b"]);

    let output = newgit(&repo, &["tracker", "list"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("`staging-db` needs fixing"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_str(&newgit_ok(&repo, &["tracker", "list", "--json"])).expect("json");
    assert_eq!(json["problems"][0]["tracker"], "staging-db");

    newgit_ok(&repo, &["tracker", "remove", "staging-db"]);
    newgit_ok(&repo, &["status"]);
}

#[test]
fn a_fork_whose_upstream_was_deleted_can_be_removed() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    with_fork(&repo);
    std::fs::remove_file(repo.join(".newgit/trackers/local-db.toml")).expect("rm");
    let err = newgit_err(&repo, &["status"]);
    assert!(
        err.contains("not defined")
            && err.contains("git checkout -- .newgit/trackers/local-db.toml"),
        "{err}"
    );
    newgit_ok(&repo, &["tracker", "list"]);
    newgit_ok(&repo, &["tracker", "remove", "staging-db"]);
    newgit_ok(&repo, &["status"]);
}

#[test]
fn tracker_list_says_what_a_fork_came_from() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    with_fork(&repo);
    let out = newgit_ok(&repo, &["tracker", "list"]);
    let row = out
        .lines()
        .find(|line| line.contains("staging-db"))
        .expect("a row");
    assert!(row.contains("(a fork of `local-db`)"), "{out}");
    let json: serde_json::Value =
        serde_json::from_str(&newgit_ok(&repo, &["tracker", "list", "--json"])).expect("json");
    let trackers = json["trackers"].as_array().expect("trackers");
    let find = |name: &str| {
        trackers
            .iter()
            .find(|tracker| tracker["name"] == name)
            .expect("listed")
            .clone()
    };
    assert_eq!(find("staging-db")["from"], "local-db");
    assert!(find("local-db")["from"].is_null());
    assert!(json["problems"].as_array().expect("problems").is_empty());
}

#[test]
fn from_says_at_once_whether_it_names_a_tracker_or_a_repository() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let err = newgit_err(
        &repo,
        &[
            "tracker",
            "create",
            "y",
            "--from",
            "https://github.com/x/y.git",
        ],
    );
    assert!(
        err.contains("is a repository, not a tracker") && err.contains("--git and --mount <dir>"),
        "{err}"
    );
    // No --mount yet: the tracker is what is said, first.
    let err = newgit_err(
        &repo,
        &[
            "tracker",
            "create",
            "staging-db",
            "--git",
            "--from",
            "local-db",
        ],
    );
    assert!(
        err.contains("`local-db` is a tracker here")
            && err.contains("newgit tracker create staging-db --from local-db"),
        "{err}"
    );
}
