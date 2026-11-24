//! A port newgit claimed for one instance, held by another instance's
//! process. Found by an agent running three instances of a Remix app: the
//! first instance's `remix dev` picked a second port for itself, the one
//! claimed for the second instance, whose server then failed to bind with
//! only a bare error in its log; and `ports --check` called the port "ok,
//! claimed by" its claimant. Worse, a claimant whose port answers reads as
//! up, whoever answers.
//!
//! `status` must say whose process holds the port (pid, directory,
//! instance) and not call the claimant ready; `ports --check` must FAIL the
//! claim naming the holder, exit 1 with `--json` too, and leave alone a
//! listener no instance owns on a port no instance claims.
//!
//! These drive the binary, since what is under test is what it prints.

use std::process::{Child, Command};

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

/// A directory under the OS temp dir. On drop it kills the stand-in tool and
/// stops both instances, then removes the directory.
struct Fixture {
    root: Utf8PathBuf,
    hog: Option<Child>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(hog) = &mut self.hog {
            let _ = hog.kill();
            let _ = hog.wait();
        }
        for instance in ["a", "b"] {
            let _ = Command::new(env!("CARGO_BIN_EXE_newgit"))
                .current_dir(self.root.join("store").as_str())
                .args(["stop", instance])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!("newgit-held-port-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    Fixture { root, hog: None }
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn json(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "`newgit {args:?}` stdout is not one JSON document ({error}):\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code(), value)
}

/// Where newgit's port search starts: a port the OS just handed out, so two
/// runs at once do not search the same range (#276).
fn free_port() -> u16 {
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("an OS-assigned port")
            .port();
        if port < 60000 {
            return port;
        }
    }
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// Serves on its claimed port, except in instance `b`, where it sits and
/// never binds: as a server does that is still building, or has already
/// failed to bind.
const SERVE: &str = r#"import os, socket, time
if os.path.basename(os.getcwd()) != "b":
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", int(os.environ["APP_PORT"])))
    s.listen()
time.sleep(600)
"#;

/// Up means something accepts on the claimed port: the check an agent
/// would write, and the one a stranger on the port fools.
const CHECK: &str = r#"import os, socket
socket.create_connection(("127.0.0.1", int(os.environ["APP_PORT"])), 1).close()
"#;

fn resource(start: u16) -> String {
    format!(
        r#"ownership = "branch"

[ports]
app = {{ start = {start}, env = "APP_PORT" }}

[actions.dev]
command = "exec python3 serve.py"
long_running = true
autostart = true

[ready]
command = "python3 check.py"
timeout_seconds = 1
"#
    )
}

fn claimed(repo: &Utf8Path, instance: &str) -> u16 {
    let (_, ports) = json(repo, &["ports", "--json"]);
    ports["ports"]
        .as_array()
        .expect("ports")
        .iter()
        .find(|claim| claim["instance"] == instance)
        .and_then(|claim| claim["port"].as_u64())
        .unwrap_or_else(|| panic!("`{instance}` claims no port: {ports}")) as u16
}

fn wait_listening(port: u16) {
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("nothing came up on {port}");
}

#[test]
fn a_claim_held_by_another_instances_process_is_named() {
    if !has_python() {
        return;
    }
    let mut fixture = fixture();
    let temp = fixture.root.clone();
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("serve.py"), SERVE).expect("write");
    std::fs::write(repo.join("check.py"), CHECK).expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    std::fs::write(
        store.paths().resources.join("app.toml"),
        resource(free_port()),
    )
    .expect("write resource");

    let (code, a) = json(&repo, &["spawn", "a", "--json"]);
    assert_eq!(code, Some(0), "{a}");
    let a_workspace = a["workspace"].as_str().expect("workspace").to_owned();
    let (code, b) = json(&repo, &["spawn", "b", "--json"]);
    assert_eq!(code, Some(1), "`b` never binds, so it is not up: {b}");
    let b_port = claimed(&repo, "b");

    // A tool of `a`'s takes `b`'s claimed port for itself.
    let hog = Command::new("python3")
        .args([
            "-c",
            &format!(
                "import socket, time\ns = socket.socket()\ns.bind(('127.0.0.1', {b_port}))\n\
                 s.listen()\ntime.sleep(60)"
            ),
        ])
        .current_dir(&a_workspace)
        .spawn()
        .expect("python3 runs");
    let hog_pid = hog.id();
    fixture.hog = Some(hog);
    wait_listening(b_port);

    // Something answers on `b`'s port, and its check passes, but it is not
    // `b`: not ready, and `status` says whose it is.
    let (_, status) = json(&repo, &["status", "b", "--json"]);
    let instance = &status["instances"][0];
    assert_ne!(instance["status"], "ready", "{status}");
    let problems = instance["problems"].to_string();
    assert!(problems.contains(&format!("pid {hog_pid}")), "{problems}");
    assert!(problems.contains("instance `a`"), "{problems}");
    assert!(problems.contains(&b_port.to_string()), "{problems}");

    let (_, said) = newgit(&repo, &["status", "b"]);
    assert!(said.contains(&format!("pid {hog_pid}")), "{said}");

    // An unrelated listener on a port nobody claims is none of `--check`'s
    // business.
    let unrelated = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let unrelated = unrelated.local_addr().expect("addr").port();

    let (code, check) = json(&repo, &["ports", "--check", "--json"]);
    assert_eq!(code, Some(1), "{check}");
    assert_eq!(check["ok"], false, "{check}");
    let ports = check["ports"].as_array().expect("ports");
    let held = ports
        .iter()
        .find(|entry| entry["port"] == b_port)
        .unwrap_or_else(|| panic!("no entry for {b_port}: {check}"));
    assert_eq!(held["verdict"], "fail", "{check}");
    assert_eq!(held["instance"], "b", "{check}");
    assert_eq!(held["holders"][0]["pid"], hog_pid, "{check}");
    assert_eq!(held["holders"][0]["instance"], "a", "{check}");
    let a_port = claimed(&repo, "a");
    let own = ports
        .iter()
        .find(|entry| entry["port"] == a_port)
        .unwrap_or_else(|| panic!("no entry for {a_port}: {check}"));
    assert_eq!(own["verdict"], "ok", "{check}");
    assert!(
        ports.iter().all(|entry| entry["port"] != unrelated),
        "{check}"
    );
    assert_eq!(
        ports.iter().filter(|entry| entry["port"] == b_port).count(),
        1,
        "{check}"
    );

    let (code, said) = newgit(&repo, &["ports", "--check"]);
    assert_eq!(code, Some(1), "{said}");
    assert!(said.contains(&format!("FAIL  {b_port}")), "{said}");
    assert!(said.contains("instance `a`"), "{said}");
}
