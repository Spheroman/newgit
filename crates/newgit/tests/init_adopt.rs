//! `newgit init` where `.newgit/` already is — a plain `git clone` of a
//! newgit project, or a project initialized before trackers had histories —
//! adopts it: root gets the project's history, as a fresh `init` gives it.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};

fn run(program: &str, dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(dir.as_str())
        .args(args)
        .env("HOME", dir.join("..").as_str())
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("runs")
}

fn ok(program: &str, dir: &Utf8Path, args: &[&str]) -> String {
    let output = run(program, dir, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

const NEWGIT: &str = env!("CARGO_BIN_EXE_newgit");

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-init-adopt-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

#[test]
fn init_in_a_plain_clone_gives_root_the_project_history_once() {
    let (_guard, temp) = tempdir();
    let origin = temp.join("origin");
    std::fs::create_dir_all(&origin).expect("mkdir");
    ok("git", &origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("README.md"), "hello\n").expect("write");
    ok("git", &origin, &["add", "."]);
    ok("git", &origin, &["commit", "-q", "-m", "initial"]);
    ok(NEWGIT, &origin, &["init"]);
    // Committed as a v1 project committed it: config, no tracker definitions.
    std::fs::remove_dir_all(origin.join(".newgit/trackers")).expect("rm");
    ok("git", &origin, &["add", "-A"]);
    ok("git", &origin, &["commit", "-q", "-m", "newgit"]);

    ok("git", &temp, &["clone", "-q", origin.as_str(), "clone"]);
    let clone = temp.join("clone");

    let out = ok(NEWGIT, &clone, &["init"]);
    assert!(out.contains("Adopted the existing .newgit/"), "{out}");
    assert!(out.contains("wrote .newgit/trackers/root.toml"), "{out}");
    assert!(out.contains("gave `root` this project's history"), "{out}");

    let log = ok(NEWGIT, &clone, &["log", "root", "--", "--format=%s"]);
    assert!(log.contains("initial"), "{log}");

    let out = ok(NEWGIT, &clone, &["init"]);
    assert!(out.contains("nothing to do"), "{out}");
}

#[test]
fn init_in_a_workspace_is_refused() {
    let (_guard, temp) = tempdir();
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    ok("git", &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    ok("git", &store, &["add", "."]);
    ok("git", &store, &["commit", "-q", "-m", "initial"]);
    ok(NEWGIT, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let text = std::fs::read_to_string(&config).expect("config");
    std::fs::write(
        &config,
        format!(
            "{text}\n[workspace]\nroot = \"{}\"\n",
            temp.join("workspaces")
        ),
    )
    .expect("write");
    ok("git", &store, &["add", "-A"]);
    ok("git", &store, &["commit", "-q", "-m", "newgit"]);
    ok(NEWGIT, &store, &["spawn", "feature"]);

    // A configured `[workspace] root` holds the workspaces directly, with no
    // per-project directory in between, so the path is known. Walking the
    // directory instead found the workspace's own contents, in whatever order
    // `read_dir` returned them, and picked a file as the cwd on Linux (#204).
    let workspace = temp.join("workspaces").join("feature");
    assert!(workspace.is_dir(), "{workspace} is not a workspace");
    let output = run(NEWGIT, &workspace, &["init"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("run `newgit init` in the project's store"),
        "{stderr}"
    );
}
