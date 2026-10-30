//! #261 and #262: `status` and `spawn` say what is actually wrong.
//!
//! #261: a supervised process that ends on its own is not `stopped`. How it
//! ended goes into its log and into `status`, an `autostart` one that died
//! makes the instance not ready, and `newgit start` brings it back.
//!
//! #262: a resource that failed to render or to resolve an export says why
//! in the human output of `spawn` and `status`, and the retry it is given is
//! one it has: fix the file or definition, then `newgit resource rebind`.
//!
//! These drive the binary, since what is under test is what it prints.

#![allow(dead_code)]

use std::process::Command;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::Value;

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

/// A directory under the OS temp dir, unique per call. On drop it stops
/// whatever the instance still runs, then removes the directory.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(self.0.join("store").as_str())
            .args(["stop", "x"])
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-truth-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

/// A store whose committed `app.conf` holds `127.0.0.1:8000` twice.
fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(
        repo.join("app.conf"),
        "listen 127.0.0.1:8000\nupstream 127.0.0.1:8000\n",
    )
    .expect("write");
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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf8 stdout"),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// `newgit status x`, once its resource column says `want`, or the last one
/// after a few seconds.
fn status_once(repo: &Utf8Path, want: &str) -> Run {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let run = newgit(repo, &["status", "x"]);
        if run.stdout.contains(want) || Instant::now() >= deadline {
            return run;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Every log of the instance, concatenated.
fn logs(store: &MetadataStore) -> String {
    let dir = store.paths().logs.join("x");
    let mut text = String::new();
    for entry in std::fs::read_dir(&dir).expect("logs dir") {
        text.push_str(&std::fs::read_to_string(entry.expect("entry").path()).unwrap_or_default());
    }
    text
}

fn server(command: &str) -> String {
    format!(
        r#"ownership = "branch"

[actions.serve]
command = "{command}"
long_running = true
autostart = true
"#
    )
}

#[test]
fn a_process_that_exits_on_its_own_is_shown_as_exited_and_makes_the_instance_not_ready() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", &server("sleep 1; exit 3"));
    let repo = store.paths().project_root.clone();

    let spawned = newgit(&repo, &["spawn", "x"]);
    assert_eq!(spawned.code, Some(0), "{}", spawned.all());

    let status = status_once(&repo, "app:exited(3)");
    assert!(status.stdout.contains("app:exited(3)"), "{}", status.stdout);
    assert!(status.stdout.contains("not ready"), "{}", status.stdout);
    assert!(
        status
            .stdout
            .contains("`app.serve` exited 3 without being stopped")
            && status.stdout.contains("`newgit action app.serve`"),
        "{}",
        status.stdout
    );
    assert!(
        logs(&store).contains("[newgit] exited 3"),
        "{}",
        logs(&store)
    );

    // `start` owes it as much as what `newgit stop` stopped.
    let started = newgit(&repo, &["start", "x"]);
    assert_eq!(started.code, Some(0), "{}", started.all());
    assert!(started.stdout.contains("app.serve"), "{}", started.stdout);
    assert!(!started.stdout.contains("Nothing"), "{}", started.stdout);
}

#[test]
fn a_process_killed_by_a_signal_says_which() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", &server("sleep 1; kill -9 $$"));
    let repo = store.paths().project_root.clone();

    let spawned = newgit(&repo, &["spawn", "x"]);
    assert_eq!(spawned.code, Some(0), "{}", spawned.all());

    let status = status_once(&repo, "app:killed(SIGKILL)");
    assert!(
        status.stdout.contains("app:killed(SIGKILL)"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("was killed by SIGKILL"),
        "{}",
        status.stdout
    );
    assert!(
        logs(&store).contains("[newgit] killed by SIGKILL"),
        "{}",
        logs(&store)
    );
}

#[test]
fn a_process_newgit_stopped_is_stopped_and_not_a_problem() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "app", &server("sleep 1000"));
    let repo = store.paths().project_root.clone();

    let spawned = newgit(&repo, &["spawn", "x"]);
    assert_eq!(spawned.code, Some(0), "{}", spawned.all());
    let stopped = newgit(&repo, &["stop", "x"]);
    assert_eq!(stopped.code, Some(0), "{}", stopped.all());

    let status = newgit(&repo, &["status", "x"]);
    assert!(status.stdout.contains("app:stopped"), "{}", status.stdout);
    assert!(!status.stdout.contains("not ready"), "{}", status.stdout);
    assert!(
        logs(&store).contains("[newgit] stopped by newgit"),
        "{}",
        logs(&store)
    );
}

/// The committed `app.conf` holds the `find` twice; the render wants once.
const BAD_RENDER: &str = r#"ownership = "branch"

[ports]
web = { start = 54800 }

[[render]]
path = "app.conf"
replace = [{ find = "127.0.0.1:8000", with = "127.0.0.1:{{ports.web}}" }]

[actions.serve]
command = "sleep 1000"
long_running = true
autostart = true
"#;

#[test]
fn a_render_failure_is_printed_with_a_retry_the_resource_has() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "web", BAD_RENDER);
    let repo = store.paths().project_root.clone();

    let spawned = newgit(&repo, &["spawn", "x"]);
    assert_eq!(spawned.code, Some(1), "{}", spawned.all());
    // What "(see below)" promises is below it: the error and its fix.
    let below = spawned
        .stdout
        .split("spawned with failures")
        .nth(1)
        .unwrap_or_else(|| panic!("{}", spawned.stdout));
    assert!(below.contains("occurs 2 time(s)"), "{}", spawned.stdout);
    assert!(
        below.contains("`newgit resource rebind web`"),
        "{}",
        spawned.stdout
    );
    assert!(
        !spawned.stdout.contains("web.prepare"),
        "{}",
        spawned.stdout
    );

    let status = newgit(&repo, &["status", "x"]);
    assert!(status.stdout.contains("web:failed"), "{}", status.stdout);
    assert!(
        status.stdout.contains("failed to render")
            && status.stdout.contains("occurs 2 time(s)")
            && status.stdout.contains("`newgit resource rebind web`"),
        "{}",
        status.stdout
    );
    assert!(!status.stdout.contains("web.prepare"), "{}", status.stdout);

    let (_, json) = {
        let run = newgit(&repo, &["spawn", "y", "--json"]);
        (
            run.code,
            serde_json::from_str::<Value>(&run.stdout).expect("json"),
        )
    };
    assert!(
        json["resources"][0]["render_error"]
            .as_str()
            .is_some_and(|error| error.contains("occurs 2 time(s)")),
        "{json}"
    );
    let _ = newgit(&repo, &["stop", "y"]);

    // Fixed in the definition, then bound again: it comes up, and starts.
    write_resource(
        &store,
        "web",
        &BAD_RENDER
            .replace(
                "find = \"127.0.0.1:8000\"",
                "find = \"listen 127.0.0.1:8000\"",
            )
            .replace(
                "with = \"127.0.0.1:{{ports.web}}\"",
                "with = \"listen 127.0.0.1:{{ports.web}}\"",
            ),
    );
    let rebound = newgit(&repo, &["resource", "rebind", "web", "x"]);
    assert_eq!(rebound.code, Some(0), "{}", rebound.all());
    assert!(rebound.stdout.contains("web.serve"), "{}", rebound.stdout);
    let status = newgit(&repo, &["status", "x"]);
    assert!(status.stdout.contains("web:running"), "{}", status.stdout);
    assert!(
        !status.stdout.contains("failed to render"),
        "{}",
        status.stdout
    );

    // Running, it is not bound again under itself.
    let refused = newgit(&repo, &["resource", "rebind", "web", "x"]);
    assert_ne!(refused.code, Some(0), "{}", refused.all());
    assert!(refused.all().contains("is running"), "{}", refused.all());
}

#[test]
fn an_export_failure_points_at_the_exports() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(
        &store,
        "web",
        r#"ownership = "branch"

[exports]
WEB_URL = "http://127.0.0.1:{{ports.nope}}"
"#,
    );
    let repo = store.paths().project_root.clone();

    let spawned = newgit(&repo, &["spawn", "x"]);
    assert_eq!(spawned.code, Some(1), "{}", spawned.all());

    let status = newgit(&repo, &["status", "x"]);
    assert!(
        status.stdout.contains("failed to bind")
            && status.stdout.contains("WEB_URL")
            && status.stdout.contains("fix `[exports]`")
            && status.stdout.contains("`newgit resource rebind web`"),
        "{}",
        status.stdout
    );
    assert!(!status.stdout.contains("web.prepare"), "{}", status.stdout);
}
