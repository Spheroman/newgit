//! The project repo, the store (`.newgit/local/trackers.git`), and each
//! workspace clone keep a real copy of every object their refs reach: refs
//! move between them without a fetch when the objects are there already,
//! but nothing is borrowed. So deleting a workspace never loses anything the
//! store needs, and the copies carry the refs they name and no tags.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};

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
    let dir =
        std::env::temp_dir().join(format!("newgit-local-ref-sync-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

struct Fixture {
    home: Utf8PathBuf,
    project: Utf8PathBuf,
}

impl Fixture {
    fn newgit(&self, dir: &Utf8Path, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(dir.as_str())
            .args(args)
            .env("HOME", self.home.as_str())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("newgit runs");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "newgit {args:?} failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    fn store(&self) -> Utf8PathBuf {
        self.project.join(".newgit/local/trackers.git")
    }

    fn workspace(&self, name: &str) -> Utf8PathBuf {
        let root = self.home.join(".newgit/workspaces");
        for entry in std::fs::read_dir(&root).expect("workspaces") {
            let candidate = Utf8PathBuf::from_path_buf(entry.expect("entry").path())
                .expect("utf8")
                .join(name);
            if candidate.is_dir() {
                return candidate;
            }
        }
        panic!("no workspace {name} under {root}");
    }
}

fn setup(temp: &Utf8Path) -> Fixture {
    let home = temp.join("home");
    std::fs::create_dir_all(&home).expect("mkdir");
    let project = temp.join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    git(&project, &["init", "-q", "-b", "main"]);
    std::fs::write(project.join("README.md"), "hello\n").expect("write");
    git(&project, &["add", "."]);
    git(&project, &["commit", "-q", "-m", "initial"]);
    let fixture = Fixture { home, project };
    fixture.newgit(&fixture.project, &["init"]);
    git(&fixture.project, &["add", "-A"]);
    git(&fixture.project, &["commit", "-q", "-m", "newgit"]);
    fixture
}

fn fsck(repo: &Utf8Path) {
    let output = git_output(repo, &["fsck", "--connectivity-only", "--no-dangling"]);
    assert!(
        output.status.success(),
        "{repo} lost objects its refs need:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn no_alternates(git_dir: &Utf8Path) {
    let alternates = git_dir.join("objects/info/alternates");
    assert!(
        !alternates.exists(),
        "{git_dir} borrows objects through {alternates}"
    );
}

#[test]
fn deleting_a_workspace_never_loses_what_the_store_needs() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.newgit(&fixture.project, &["spawn", "feature"]);
    let workspace = fixture.workspace("feature");

    std::fs::write(workspace.join("README.md"), "changed\n").expect("write");
    git(&workspace, &["commit", "-q", "-am", "change"]);
    fixture.newgit(&workspace, &["checkpoint"]);
    // Uncommitted work too, and a checkpoint with nothing new to fetch.
    std::fs::write(workspace.join("notes.txt"), "draft\n").expect("write");
    fixture.newgit(&workspace, &["checkpoint"]);
    fixture.newgit(&workspace, &["checkpoint"]);

    no_alternates(&fixture.project.join(".git"));
    no_alternates(&fixture.store());
    no_alternates(&workspace.join(".git"));

    std::fs::remove_dir_all(&workspace).expect("rm -rf the workspace");
    // Git's own cleanup, with nothing protected by grace periods.
    git(&fixture.project, &["gc", "-q", "--prune=now"]);
    git(&fixture.store(), &["gc", "-q", "--prune=now"]);

    fsck(&fixture.project);
    fsck(&fixture.store());
    for id in ["ckpt_001", "ckpt_002", "ckpt_003"] {
        let name = format!("refs/newgit/checkpoints/feature/{id}");
        let output = git_output(
            &fixture.project,
            &["rev-list", "--objects", "--quiet", &name],
        );
        assert!(
            output.status.success(),
            "{name} is incomplete after the workspace went: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn undo_restores_a_checkpoint_the_workspace_still_holds() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.newgit(&fixture.project, &["spawn", "feature"]);
    let workspace = fixture.workspace("feature");

    std::fs::write(workspace.join("README.md"), "one\n").expect("write");
    git(&workspace, &["commit", "-q", "-am", "one"]);
    fixture.newgit(&workspace, &["checkpoint"]);
    let one = git(&workspace, &["rev-parse", "HEAD"]);
    std::fs::write(workspace.join("README.md"), "two\n").expect("write");
    git(&workspace, &["commit", "-q", "-am", "two"]);
    fixture.newgit(&workspace, &["checkpoint"]);

    fixture.newgit(&workspace, &["undo", "--to", "ckpt_001"]);
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), one);
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).expect("read"),
        "one\n"
    );
    fsck(&workspace);
}

#[test]
fn a_project_tag_stays_out_of_the_store_and_stays_deleted() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    git(&fixture.project, &["tag", "v1", "HEAD~1"]);
    fixture.newgit(&fixture.project, &["spawn", "first"]);
    assert_eq!(
        git(&fixture.store(), &["for-each-ref", "refs/tags/"]),
        "",
        "a project tag was copied into the store"
    );

    git(&fixture.project, &["tag", "-d", "v1"]);
    fixture.newgit(&fixture.project, &["spawn", "second"]);
    assert_eq!(
        git(&fixture.project, &["for-each-ref", "refs/tags/"]),
        "",
        "a deleted tag came back"
    );
}
