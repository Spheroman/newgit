//! `newgit spawn --from-checkpoint` of a checkpoint taken on this machine
//! needs no remote: the record, its commits, and its tracker revs are all
//! here already. It used to refuse without a `[remote]`.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};

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

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "newgit-spawn-local-checkpoint-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

struct Fixture {
    temp: Utf8PathBuf,
    store: Utf8PathBuf,
}

impl Fixture {
    fn newgit(&self, dir: &Utf8Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(dir.as_str())
            .args(args)
            .env("HOME", self.temp.join("home").as_str())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("newgit runs")
    }

    fn ok(&self, dir: &Utf8Path, args: &[&str]) -> String {
        let output = self.newgit(dir, args);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "newgit {args:?} failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    fn workspace(&self, name: &str) -> Utf8PathBuf {
        Utf8PathBuf::from(self.ok(&self.store, &["status", name, "--path"]).trim())
    }
}

/// A project with no remote, and `feature-a` checkpointed as
/// `feature-a/ckpt_001` with a commit, uncommitted work, and content in a
/// tracker outside Git.
fn checkpointed(temp: &Utf8Path) -> Fixture {
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    git(&store, &["add", "."]);
    git(&store, &["commit", "-q", "-m", "initial"]);
    let fixture = Fixture {
        temp: temp.to_path_buf(),
        store,
    };
    fixture.ok(&fixture.store, &["init"]);
    let config = fixture.store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "runtime-env", "--audience", "user"],
    );
    fixture.ok(
        &fixture.store,
        &["tracker", "track", "runtime-env", ".env.local"],
    );
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "newgit"]);

    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let author = fixture.workspace("feature-a");
    std::fs::write(author.join("feature.txt"), "committed\n").expect("write");
    git(&author, &["add", "feature.txt"]);
    git(&author, &["commit", "-q", "-m", "feature"]);
    std::fs::write(author.join("wip.txt"), "uncommitted\n").expect("write");
    std::fs::write(author.join(".env.local"), "KEY=value\n").expect("write");
    fixture.ok(&author, &["checkpoint"]);
    fixture
}

fn assert_came_up_as_the_checkpoint(fixture: &Fixture, spawned: &str) {
    assert!(
        spawned.contains("Spawned `review` from feature-a/ckpt_001, taken on this machine"),
        "{spawned}"
    );
    let review = fixture.workspace("review");
    assert_eq!(
        std::fs::read_to_string(review.join("feature.txt")).expect("committed work"),
        "committed\n"
    );
    assert_eq!(
        std::fs::read_to_string(review.join("wip.txt")).expect("uncommitted work"),
        "uncommitted\n"
    );
    assert_eq!(
        std::fs::read_to_string(review.join(".env.local")).expect("tracker content"),
        "KEY=value\n"
    );
    assert_eq!(
        git(&review, &["symbolic-ref", "--short", "HEAD"]),
        "review",
        "on its own branch"
    );
    let checkpoints = fixture.ok(&fixture.store, &["checkpoints", "review"]);
    assert!(
        checkpoints.contains("spawned from feature-a/ckpt_001"),
        "{checkpoints}"
    );
}

#[test]
fn a_checkpoint_on_this_machine_spawns_with_no_remote() {
    let (_guard, temp) = tempdir();
    let fixture = checkpointed(&temp);

    let spawned = fixture.ok(
        &fixture.store,
        &["spawn", "review", "--from-checkpoint", "feature-a/ckpt_001"],
    );

    assert_came_up_as_the_checkpoint(&fixture, &spawned);
}

#[test]
fn a_checkpoint_outlives_its_instance_and_still_spawns() {
    let (_guard, temp) = tempdir();
    let fixture = checkpointed(&temp);
    fixture.ok(&fixture.store, &["remove", "feature-a"]);

    let spawned = fixture.ok(
        &fixture.store,
        &["spawn", "review", "--from-checkpoint", "feature-a/ckpt_001"],
    );

    assert_came_up_as_the_checkpoint(&fixture, &spawned);
}

#[test]
fn a_checkpoint_this_machine_lacks_is_unknown_without_a_remote() {
    let (_guard, temp) = tempdir();
    let fixture = checkpointed(&temp);

    let output = fixture.newgit(
        &fixture.store,
        &["spawn", "review", "--from-checkpoint", "feature-a/ckpt_009"],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ckpt_009"), "{stderr}");
    assert!(!stderr.contains("remote add"), "{stderr}");
    assert!(
        !fixture.store.join(".newgit/branches/review.toml").exists(),
        "nothing was spawned"
    );
}
