//! `newgit start` probes the ports a binding claimed, and moves one that
//! another program holds now to a free one (#441). The case from the issue:
//! spawn claims a port for `db`, `stop`, something else binds it, `start`.
//! Before, `start` ran the service against a port somebody else held, with
//! the rendered `.env` and the exported URL still naming it.

use std::net::TcpListener;
use std::process::Command;

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

/// Stops what the instance runs when the test ends, even on a failed
/// assertion, then removes the directory.
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
    let dir = std::env::temp_dir().join(format!("newgit-busy-port-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join(".env"), "API=http://127.0.0.1:1\n").expect("write");
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

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    Run {
        status: output.status,
        stdout: String::from_utf8(output.stdout).expect("utf8 stdout"),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn newgit_ok(repo: &Utf8Path, args: &[&str]) -> Run {
    let run = newgit(repo, args);
    assert!(
        run.status.success(),
        "newgit {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    run
}

/// The port `x`'s `db` is bound to, from the record.
fn db_port(store: &MetadataStore) -> u16 {
    store.find_branch("x").expect("instance").resources["db"].resolved_ports["api"]
}

/// `db` claims `api`, exports it as `API_URL`, renders it into `.env`, and
/// has `sleep` standing in for the service.
const DB: &str = r#"ownership = "branch"

[ports.api]
start = 47500

[exports]
API_URL = "http://127.0.0.1:{{ports.api}}"

[[render]]
path = ".env"
replace = [{ find = "API=http://127.0.0.1:1", with = "API=http://127.0.0.1:{{ports.api}}" }]

[actions.start]
command = "sleep 1000"
long_running = true
"#;

fn spawned() -> (TempDir, Utf8PathBuf, MetadataStore, Utf8PathBuf) {
    let (guard, temp) = tempdir();
    let store = setup(&temp);
    std::fs::write(store.paths().resources.join("db.toml"), DB).expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "db.start", "x"]);
    let workspace = store.find_branch("x").expect("instance").workspace_path;
    (guard, repo, store, workspace)
}

#[test]
fn start_moves_a_port_another_program_took() {
    let (_guard, repo, store, workspace) = spawned();
    let claimed = db_port(&store);
    newgit_ok(&repo, &["stop", "x"]);

    // Something else binds the claimed port while the instance is stopped,
    // held for the whole test so it cannot be handed back to anyone.
    let _held = TcpListener::bind(("0.0.0.0", claimed)).expect("another program binds the port");

    let run = newgit_ok(&repo, &["start", "x", "--json"]);
    let json: Value = serde_json::from_str(&run.stdout).expect("one JSON document");
    let moved = db_port(&store);
    assert_ne!(moved, claimed, "the held port is not kept: {json}");
    assert_eq!(
        json["moved_ports"],
        serde_json::json!([
            {"resource": "db", "port": "api", "from": claimed, "to": moved}
        ])
    );
    assert_eq!(json["resources"][0]["status"], "started", "{json}");

    // The files and the exports follow the port.
    let env = std::fs::read_to_string(workspace.join(".env")).expect("rendered .env");
    assert_eq!(env, format!("API=http://127.0.0.1:{moved}\n"));
    let record = store.find_branch("x").expect("instance");
    assert_eq!(
        record.resources["db"].resolved_exports["API_URL"],
        format!("http://127.0.0.1:{moved}")
    );
}

#[test]
fn start_says_so_in_plain_output_too() {
    let (_guard, repo, store, _) = spawned();
    let claimed = db_port(&store);
    newgit_ok(&repo, &["stop", "x"]);
    let _held = TcpListener::bind(("127.0.0.1", claimed)).expect("another program binds the port");

    let run = newgit_ok(&repo, &["start", "x"]);
    let moved = db_port(&store);
    assert!(
        run.stderr
            .contains(&format!("port api of `db` moved from {claimed} to {moved}"))
            && run.stderr.contains("another program"),
        "stderr:\n{}",
        run.stderr
    );
}

#[test]
fn start_leaves_a_free_port_alone() {
    let (_guard, repo, store, _) = spawned();
    let claimed = db_port(&store);
    newgit_ok(&repo, &["stop", "x"]);

    let run = newgit_ok(&repo, &["start", "x", "--json"]);
    let json: Value = serde_json::from_str(&run.stdout).expect("one JSON document");
    assert!(json.get("moved_ports").is_none(), "{json}");
    assert_eq!(db_port(&store), claimed);
}
