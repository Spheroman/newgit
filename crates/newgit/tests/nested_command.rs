//! A hook that runs `newgit` on its own instance (#212). The command that
//! runs the hook holds the instance's lock, so the nested one would wait for
//! its own ancestor forever; told through `NEWGIT_LOCKED`, it refuses
//! instead, and says why.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;

fn git(dir: &Utf8Path, args: &[&str]) {
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
    let dir = std::env::temp_dir().join(format!("newgit-nested-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> MetadataStore {
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
    store
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .env_remove("NEWGIT_LOCKED")
        .args(args)
        .output()
        .expect("newgit runs")
}

#[test]
fn a_hook_running_newgit_on_its_own_instance_is_refused_not_deadlocked() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let nested_stderr = temp.join("nested.stderr");
    let binary = env!("CARGO_BIN_EXE_newgit");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!(
            r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "{binary} checkpoint x -m nested 2>{nested_stderr}; echo state"
"#
        ),
    )
    .expect("write resource");

    let spawned = newgit(&repo, &["spawn", "x"]);
    assert!(
        spawned.status.success(),
        "{}",
        String::from_utf8_lossy(&spawned.stdout)
    );
    // Would hang here before the lock was announced to hooks.
    let outer = newgit(&repo, &["checkpoint", "x", "-m", "outer"]);
    assert!(
        outer.status.success(),
        "{}{}",
        String::from_utf8_lossy(&outer.stdout),
        String::from_utf8_lossy(&outer.stderr)
    );

    let nested = std::fs::read_to_string(&nested_stderr).expect("nested stderr");
    assert!(
        nested.contains("started from inside it"),
        "the nested checkpoint did not refuse: {nested}"
    );
    // Only the outer checkpoint exists.
    let list = newgit(&repo, &["checkpoints", "x"]);
    let out = String::from_utf8_lossy(&list.stdout);
    assert!(out.contains("ckpt_001"), "{out}");
    assert!(!out.contains("ckpt_002"), "{out}");
}

/// `newgit` with `NEWGIT_LOCKED` set to `locked` (or unset), started in the
/// background.
fn newgit_child(repo: &Utf8Path, args: &[&str], locked: Option<&str>) -> std::process::Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_newgit"));
    command
        .current_dir(repo.as_str())
        .env_remove("NEWGIT_LOCKED")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(locked) = locked {
        command.env("NEWGIT_LOCKED", locked);
    }
    command.spawn().expect("newgit runs")
}

/// Its output once it exits; a test fails rather than hangs on one that
/// never does.
fn finish(mut child: std::process::Child) -> std::process::Output {
    let start = std::time::Instant::now();
    while child.try_wait().expect("wait").is_none() {
        if start.elapsed() > std::time::Duration::from_secs(60) {
            let _ = child.kill();
            let output = child.wait_with_output().expect("output");
            panic!(
                "newgit never finished:\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output().expect("output")
}

/// A checkpoint of `x` started while this test process holds `x`'s lock,
/// with `locked` as its `NEWGIT_LOCKED`: it has to wait for the lock, not
/// refuse, and take its checkpoint once the lock is let go.
fn waits_for_an_unrelated_holder(locked: Option<&str>) {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let spawned = newgit(&repo, &["spawn", "x"]);
    assert!(spawned.status.success());

    let held = store.lock_instance("x", "x").expect("lock");
    let mut child = newgit_child(&repo, &["checkpoint", "x", "-m", "waited"], locked);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        child.try_wait().expect("wait").is_none(),
        "the checkpoint did not wait for the lock: {:?}",
        child.wait_with_output()
    );
    drop(held);

    let output = finish(child);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("waiting for `x`"), "{stderr}");
    let list = newgit(&repo, &["checkpoints", "x"]);
    assert!(String::from_utf8_lossy(&list.stdout).contains("ckpt_001"));
}

#[test]
fn a_command_waits_for_another_holding_the_lock() {
    waits_for_an_unrelated_holder(None);
}

/// A dev server started under a lock keeps the `NEWGIT_LOCKED` it was given
/// long after that command has finished. What it names has to match the
/// lock as it is now, not merely a process that is alive: here the value is
/// the old `<slug>:<pid>` shape naming this very (live) test process, and
/// the holder is unrelated, so it waits.
#[test]
fn a_command_whose_inherited_lock_is_stale_waits_like_any_other() {
    waits_for_an_unrelated_holder(Some(&format!("x:{}", std::process::id())));
    waits_for_an_unrelated_holder(Some(&format!("x:{}.0.0", std::process::id())));
}

/// `x`'s checkpoint hook checkpoints `y`, whose hook checkpoints `x`. The
/// innermost `newgit` is two hooks below the one holding `x`, and still has
/// to see that it is.
#[test]
fn a_hook_reaching_its_own_instance_through_another_is_refused() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let nested_stderr = temp.join("nested.stderr");
    let binary = env!("CARGO_BIN_EXE_newgit");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!(
            r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "if [ \"$NEWGIT_BRANCH\" = x ]; then {binary} checkpoint y -m via-x >/dev/null; else {binary} checkpoint x -m via-y 2>{nested_stderr}; fi; echo state"
"#
        ),
    )
    .expect("write resource");
    for name in ["x", "y"] {
        let spawned = newgit(&repo, &["spawn", name]);
        assert!(spawned.status.success());
    }

    let outer = finish(newgit_child(
        &repo,
        &["checkpoint", "x", "-m", "outer"],
        None,
    ));
    assert!(
        outer.status.success(),
        "{}{}",
        String::from_utf8_lossy(&outer.stdout),
        String::from_utf8_lossy(&outer.stderr)
    );
    let nested = std::fs::read_to_string(&nested_stderr).expect("nested stderr");
    assert!(
        nested.contains("started from inside it"),
        "the innermost checkpoint did not refuse: {nested}"
    );
    let x = newgit(&repo, &["checkpoints", "x"]);
    let x = String::from_utf8_lossy(&x.stdout);
    assert!(x.contains("ckpt_001") && !x.contains("ckpt_002"), "{x}");
    let y = newgit(&repo, &["checkpoints", "y"]);
    assert!(String::from_utf8_lossy(&y.stdout).contains("ckpt_001"));
}
