//! A spawn killed while a resource's `prepare` runs leaves a record that
//! `newgit action <resource>.prepare`, which `newgit status` offers for it,
//! can finish from.
//!
//! The record is written before the resources bind, and a resource's ports
//! are claimed by writing them into it before its `prepare` runs, but that
//! claim was all of a binding saved before `prepare`. So the record a
//! killed spawn left held a resource's ports and not its exports or its
//! rendered files: the `prepare` run from the hint ran without the
//! resource's own exports and marked it ready, and every later command went
//! without them. A resource without ports, for which nothing is claimed,
//! was not in that record at all, and `status` called the instance ready.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::branch::ResourceStatus;
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

/// A directory under the OS temp dir, unique per call and removed when the
/// returned guard drops, as in `spawn_exit.rs`.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-spawn-killed-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("app.conf"), "port = 47999\n").expect("write");
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

fn newgit(repo: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

/// A `prepare` that kills the `newgit` running it the first time, as a crash
/// would, and succeeds when it is run again from the hint.
fn prepare_killing_newgit_once(temp: &Utf8Path) -> String {
    let killed = temp.join("killed");
    format!(
        r#"[actions.prepare]
command = "if [ ! -e {killed} ]; then touch {killed}; kill -9 $PPID; fi"
"#
    )
}

fn spawn_killed_in_prepare(repo: &Utf8Path) {
    let spawn = newgit(repo, &["spawn", "a"]);
    assert_eq!(
        spawn.status.signal(),
        Some(9),
        "the first `prepare` should have killed the spawn: {:?}\n{}",
        spawn.status,
        String::from_utf8_lossy(&spawn.stderr)
    );
}

/// Run the `newgit action <resource>.prepare` that `status` offers for `a`,
/// and return `newgit env a` afterwards.
fn finish_from_the_hint(repo: &Utf8Path, resource: &str) -> String {
    let status = newgit(repo, &["status"]);
    let status = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status.contains(&format!("run `newgit action {resource}.prepare`")),
        "`status` does not offer the action this test follows:\n{status}"
    );

    let action = newgit(repo, &["action", &format!("{resource}.prepare"), "a"]);
    assert!(
        action.status.success(),
        "{}",
        String::from_utf8_lossy(&action.stderr)
    );
    String::from_utf8_lossy(&newgit(repo, &["env", "a"]).stdout).into_owned()
}

#[test]
fn a_spawn_killed_in_prepare_leaves_a_record_its_prepare_can_finish_from() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("web.toml"),
        format!(
            r#"ownership = "branch"

[ports]
app = {{ start = 47900 }}

[exports]
APP_URL = "http://127.0.0.1:{{{{ports.app}}}}"

[[render]]
path = "app.conf"
replace = [ {{ find = "port = 47999", with = "port = {{{{ports.app}}}}" }} ]

{}"#,
            prepare_killing_newgit_once(&temp)
        ),
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    spawn_killed_in_prepare(&repo);

    let record = store
        .find_branch("a")
        .expect("the killed spawn left its record");
    let web = &record.resources["web"];
    let port = web.resolved_ports["app"];
    assert_eq!(web.status, ResourceStatus::Pending);
    assert_eq!(
        web.resolved_exports.get("APP_URL").map(String::as_str),
        Some(format!("http://127.0.0.1:{port}").as_str()),
        "the record a killed spawn left holds the resource's ports but not its exports"
    );
    assert_eq!(
        web.rendered
            .iter()
            .map(|rendered| rendered.path.as_str())
            .collect::<Vec<_>>(),
        ["app.conf"],
        "the record a killed spawn left does not know `app.conf` was rendered"
    );

    let env = finish_from_the_hint(&repo, "web");
    assert!(
        env.contains(&format!("APP_URL=http://127.0.0.1:{port}")),
        "the instance came up without its exports:\n{env}"
    );
}

#[test]
fn a_spawn_killed_preparing_a_resource_without_ports_is_not_called_ready() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("deps.toml"),
        format!(
            r#"ownership = "branch"

[exports]
DEPS_MODE = "dev"

{}"#,
            prepare_killing_newgit_once(&temp)
        ),
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    spawn_killed_in_prepare(&repo);

    let record = store
        .find_branch("a")
        .expect("the killed spawn left its record");
    let deps = record
        .resources
        .get("deps")
        .expect("the record a killed spawn left has no binding for a resource without ports");
    assert_eq!(deps.status, ResourceStatus::Pending);

    let env = finish_from_the_hint(&repo, "deps");
    assert!(
        env.contains("DEPS_MODE=dev"),
        "the instance came up without its exports:\n{env}"
    );
}
