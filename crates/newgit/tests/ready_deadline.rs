//! #276: where `[ready] timeout_seconds` is a deadline, and what a resource
//! that is not up says. A definition that declares `[ready]`, or a resource
//! with no process newgit supervises, is not up once its timeout has passed,
//! and `spawn` and `start` exit 1; a supervised process with no `[ready]` is
//! `starting` for as long as it runs. A running process's lines name its log,
//! and its not-up problem ends with how that log ends.
//!
//! These drive the binary, since what is under test is what it prints.

#![allow(dead_code)]

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

/// A directory under the OS temp dir, unique per call. On drop it stops
/// whatever the instance still runs, then removes the directory.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(self.0.join("store").as_str())
            .args(["stop", "x"])
            .output();
        let _ = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(self.0.join("store").as_str())
            .args(["stop", "y"])
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-deadline-{}-{n}", std::process::id()));
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

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn json(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, Value) {
    let (code, stdout, stderr) = newgit(repo, args);
    let value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("`newgit {args:?}` stdout is not one JSON document ({error}):\n{stdout}\n{stderr}")
    });
    (code, value)
}

/// Where newgit's port search starts: a port the OS just handed out.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("an OS-assigned port")
        .port()
}

#[test]
fn a_supervised_process_with_no_ready_section_is_starting_and_spawn_exits_0() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    // No `[ready]`: nobody said how long it takes, so no deadline.
    write_resource(
        &store,
        "app",
        &format!(
            "ownership = \"branch\"\n\n[ports]\nweb = {{ start = {} }}\n\n\
             [actions.serve]\ncommand = \"echo compiling; exec sleep 600\"\nlong_running = true\nautostart = true\n",
            free_port()
        ),
    );
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{spawned}");
    assert_eq!(spawned["problems"], serde_json::json!([]), "{spawned}");
    let log = spawned["starting"][0]["log"].as_str().expect("its log");
    assert!(
        log.starts_with(spawned["logs"].as_str().expect("logs")),
        "{spawned}"
    );
    assert!(log.contains("app.serve"), "{log}");

    let (code, status, _) = newgit(&repo, &["status"]);
    assert_eq!(code, Some(0), "{status}");
    assert!(status.contains(" starting "), "{status}");
    assert!(
        status.contains("not answering yet on web (")
            && status.contains(&format!("; its log is {log}")),
        "{status}"
    );
}

/// What a dev server under `npm-run-all` looks like when it crashes: the
/// wrapper lives on, so the process is running, and only its log says why.
const CRASHED_IN_A_WRAPPER: &str = r#"ownership = "branch"

[actions.serve]
command = "(echo 'Error: Cannot find module next'; exit 1); exec sleep 600"
long_running = true
autostart = true

[ready]
command = "exit 1"
timeout_seconds = 1
"#;

#[test]
fn a_server_that_crashed_inside_a_wrapper_names_its_log_and_how_it_ends() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "web", CRASHED_IN_A_WRAPPER);
    let repo = store.paths().project_root.clone();

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(1), "{stdout}{stderr}");
    assert!(stdout.contains("  logs:      "), "{stdout}");
    assert!(stdout.contains("not ready: `x`"), "{stdout}");
    assert!(
        stdout.contains("`web` is not up after 1s (`exit 1` exited 1)")
            && stdout.contains("its log is ")
            && stdout.contains("web.serve")
            && stdout.contains("    | Error: Cannot find module next"),
        "{stdout}"
    );
}

#[test]
fn start_exits_1_when_what_it_brought_back_is_not_up() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "web", CRASHED_IN_A_WRAPPER);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(1), "{spawned}");
    let (code, stdout, stderr) = newgit(&repo, &["stop", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    let (code, started) = json(&repo, &["start", "x", "--json"]);
    assert_eq!(code, Some(1), "{started}");
    let problem = started["problems"][0].as_str().expect("a problem");
    assert!(problem.contains("`web` is not up after 1s"), "{started}");

    let (code, stdout, stderr) = newgit(&repo, &["stop", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, _) = newgit(&repo, &["start", "x"]);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(stdout.contains("not ready: `x`"), "{stdout}");
}

/// No process newgit supervises: `prepare` brings up something the check
/// then sees, and on any later run brings up nothing.
const STACK: &str = r#"ownership = "branch"

[actions.prepare]
command = "test -f once || touch once up"

[ready]
command = "test -f up"
timeout_seconds = 5
"#;

#[test]
fn an_unsupervised_resource_is_starting_within_its_timeout_and_not_up_after() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    write_resource(&store, "stack", STACK);
    let repo = store.paths().project_root.clone();

    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{spawned}");
    let workspace = Utf8PathBuf::from(spawned["workspace"].as_str().expect("workspace"));

    // It goes down, and a `prepare` by hand does not bring it back: its
    // timeout runs from that `prepare`.
    std::fs::remove_file(workspace.join("up")).expect("rm up");
    let (code, stdout, stderr) = newgit(&repo, &["action", "stack.prepare", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let prepared = std::time::Instant::now();

    let (_, status, _) = newgit(&repo, &["status"]);
    assert!(
        prepared.elapsed() < std::time::Duration::from_secs(5),
        "too slow to tell"
    );
    assert!(status.contains(" starting "), "{status}");
    assert!(
        status.contains("`stack` is still starting: `test -f up` exited 1"),
        "{status}"
    );

    std::thread::sleep(std::time::Duration::from_millis(5500).saturating_sub(prepared.elapsed()));
    let (_, status, _) = newgit(&repo, &["status"]);
    assert!(status.contains(" not ready "), "{status}");
    assert!(
        status.contains("`stack` is not up after 5s (`test -f up` exited 1)")
            && status.contains("newgit action stack.prepare"),
        "{status}"
    );
}

#[test]
fn status_of_one_instance_runs_only_its_checks() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    // Each run of the check leaves a line behind in its own workspace.
    write_resource(
        &store,
        "stack",
        "ownership = \"branch\"\n\n[ready]\ncommand = \"echo asked >> asked\"\n",
    );
    let repo = store.paths().project_root.clone();
    let (code, x) = json(&repo, &["spawn", "x", "--json"]);
    assert_eq!(code, Some(0), "{x}");
    let (code, y) = json(&repo, &["spawn", "y", "--json"]);
    assert_eq!(code, Some(0), "{y}");
    let asked = |spawned: &Value| {
        let workspace = Utf8PathBuf::from(spawned["workspace"].as_str().expect("workspace"));
        std::fs::read_to_string(workspace.join("asked"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    let (x_before, y_before) = (asked(&x), asked(&y));

    let (code, status, _) = newgit(&repo, &["status", "x"]);
    assert_eq!(code, Some(0), "{status}");
    assert_eq!(asked(&x), x_before + 1, "{status}");
    assert_eq!(asked(&y), y_before, "`status x` ran `y`'s check");
}

/// A CA of the test's own and a certificate for 127.0.0.1 it signed: what
/// mkcert makes, without the step that makes this machine trust it.
fn untrusted_certificate(dir: &Utf8Path) -> bool {
    let run = |args: &[&str]| {
        Command::new("openssl")
            .current_dir(dir.as_str())
            .args(args)
            .output()
            .is_ok_and(|output| output.status.success())
    };
    std::fs::write(
        dir.join("ext"),
        "subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n",
    )
    .expect("write ext");
    run(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        "ca.key",
        "-out",
        "ca.pem",
        "-days",
        "1",
        "-subj",
        "/CN=newgit test CA",
    ]) && run(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        "key.pem",
        "-out",
        "leaf.csr",
        "-subj",
        "/CN=127.0.0.1",
    ]) && run(&[
        "x509",
        "-req",
        "-in",
        "leaf.csr",
        "-CA",
        "ca.pem",
        "-CAkey",
        "ca.key",
        "-CAcreateserial",
        "-out",
        "cert.pem",
        "-days",
        "1",
        "-extfile",
        "ext",
    ])
}

#[test]
fn an_https_check_against_a_certificate_this_machine_does_not_trust_says_so() {
    let (_guard, temp) = tempdir();
    let has_python = Command::new("python3").arg("--version").output().is_ok();
    if !has_python || !untrusted_certificate(&temp) {
        return;
    }
    let port = free_port();
    let mut server = Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import http.server, ssl\n\
             s = http.server.HTTPServer(('127.0.0.1', {port}), http.server.SimpleHTTPRequestHandler)\n\
             c = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)\n\
             c.load_cert_chain('{temp}/cert.pem', '{temp}/key.pem')\n\
             s.socket = c.wrap_socket(s.socket, server_side=True)\n\
             s.serve_forever()\n"
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 runs");
    // Listening before the spawn asks, however slowly Python starts.
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listening = (0..300).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::net::TcpStream::connect(address).is_ok()
    });
    assert!(listening, "the https server never listened");

    let store = setup(&temp);
    write_resource(
        &store,
        "web",
        &format!(
            "ownership = \"branch\"\n\n[ready]\nhttp = \"https://127.0.0.1:{port}/\"\ntimeout_seconds = 1\n"
        ),
    );
    let repo = store.paths().project_root.clone();
    let (code, spawned) = json(&repo, &["spawn", "x", "--json"]);
    let _ = server.kill();
    let _ = server.wait();
    assert_eq!(code, Some(1), "{spawned}");
    let problem = spawned["problems"][0].as_str().expect("a problem");
    assert!(
        problem.contains("this machine does not trust its certificate")
            && problem.contains("mkcert -install"),
        "{problem}"
    );
}
