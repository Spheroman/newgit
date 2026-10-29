//! A role switch parks the resources it takes away (#254, #255).
//!
//! `branch-db` brings `db`, a resource newgit does not supervise: its
//! `prepare` starts a detached listener on its port, the way `supabase start`
//! brings up a stack, and its `suspend` and `[cleanup]` take it down. Like
//! `supabase start`, `prepare` exits 0 without touching anything when its
//! listener is already up, so a re-bind on new ports would report it ready
//! while it listened on the old ones.
//!
//! Dropping `branch-db` (here, by `use`-ing the other tracker in its role)
//! must take `db` down and say how, keep its ports, and bring it back on them
//! when `branch-db` is used again. `remove` must tear down a binding parked
//! that way, which it used not to see.

use std::process::Command;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use newgit_core::{ParkedState, SourceSubstrate};

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

/// A directory under the OS temp dir. On drop it kills whatever listener
/// the stand-in left behind, so a failed assertion leaks nothing.
struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Some(pid) = listener_pid(&self.0) {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-drop-parks-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

/// The stand-in for an unsupervised stack. `up` starts a detached listener
/// on the port unless one it started is still alive, in which case it says
/// so and exits 0, as `supabase start` does; `down` kills it. Every call is
/// appended to `calls`, so the test reads which hooks ran.
const STACK: &str = r#"#!/bin/sh
state="$1"; verb="$2"; port="$3"
echo "$verb $port" >> "$state/calls"
alive() { [ -f "$state/pid" ] && kill -0 "$(cat "$state/pid")" 2>/dev/null; }
case "$verb" in
  up)
    if alive; then echo "already running on $(cat "$state/port")"; exit 0; fi
    nohup python3 -c 'import socket, sys, time
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1]))); s.listen(8)
while True: time.sleep(60)' "$port" >/dev/null 2>&1 &
    echo $! > "$state/pid"; echo "$port" > "$state/port"
    for _ in $(seq 50); do
      python3 -c 'import socket, sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.2)' "$port" 2>/dev/null && exit 0
      sleep 0.1
    done
    exit 1 ;;
  down)
    if alive; then kill "$(cat "$state/pid")"; fi
    rm -f "$state/pid" ;;
esac
"#;

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

    let state = temp.join("stack");
    std::fs::create_dir_all(&state).expect("mkdir state");
    let script = temp.join("stack.sh");
    std::fs::write(&script, STACK).expect("write script");
    let stack = format!("sh {script} {state}");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!(
            r#"ownership = "branch"

[ports.api]
start = 47310

[actions.prepare]
command = "{stack} up {{{{ports.api}}}}"

[actions.suspend]
command = "{stack} down {{{{ports.api}}}}"

[actions.resume]
command = "{stack} up {{{{ports.api}}}}"

[cleanup]
command = "{stack} down {{{{ports.api}}}}"
"#
        ),
    )
    .expect("write resource");
    std::fs::write(
        store.paths().trackers.join("local-db.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.local-db\"]\nrole = \"db\"\npriority = 1\n",
    )
    .expect("write tracker");
    std::fs::write(
        store.paths().trackers.join("branch-db.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.branch-db\"]\nrole = \"db\"\npriority = -1\nresources = [\"db\"]\n",
    )
    .expect("write tracker");
    store
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

fn listener_pid(temp: &Utf8Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(temp.join("stack/pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success());
    alive.then_some(pid)
}

fn answering(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// A killed listener can take a moment to let go of its port.
fn settles_down(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !answering(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn calls(temp: &Utf8Path) -> Vec<String> {
    std::fs::read_to_string(temp.join("stack/calls"))
        .unwrap_or_default()
        .lines()
        .map(|line| line.split(' ').next().unwrap_or_default().to_owned())
        .collect()
}

fn db_port(store: &MetadataStore) -> u16 {
    let branch = store.find_branch("x").expect("instance");
    let binding = branch
        .resources
        .get("db")
        .or_else(|| branch.parked.get("db").map(|parked| &parked.binding))
        .expect("db bound or parked");
    binding.resolved_ports["api"]
}

#[test]
fn a_role_switch_parks_the_resources_it_takes_away_and_remove_tears_them_down() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    newgit(&repo, &["spawn", "x", "--use", "branch-db"]);
    let port = db_port(&store);
    assert!(answering(port), "prepare brought the stack up on {port}");
    assert_eq!(calls(&temp), ["up"]);

    // Switch the role away: `db` is taken down by its `suspend`, and says so.
    let switched = newgit(&repo, &["tracker", "use", "local-db", "x"]);
    assert!(
        switched.contains("parked `db`: ran `suspend`"),
        "the drop says which hook ran:\n{switched}"
    );
    assert!(settles_down(port), "the stack is down after the switch");
    assert!(listener_pid(&temp).is_none());
    let branch = store.find_branch("x").expect("instance");
    assert!(!branch.resources.contains_key("db"));
    assert_eq!(branch.parked["db"].state, ParkedState::Suspended);
    let ports = newgit(&repo, &["ports"]);
    assert!(
        ports.contains(&format!("db.api:{port} (parked)")),
        "a parked binding still claims its port:\n{ports}"
    );

    // And back: the same port, resumed rather than prepared, listening.
    let back = newgit(&repo, &["tracker", "use", "branch-db", "x"]);
    assert!(back.contains("resumed `db`"), "{back}");
    assert_eq!(db_port(&store), port, "the parked port is handed back");
    assert!(answering(port), "the stack is up on the port it was given");
    let branch = store.find_branch("x").expect("instance");
    assert!(branch.parked.is_empty());
    assert_eq!(calls(&temp), ["up", "down", "up"]);

    // Park it again, then remove the instance: the parked binding is torn
    // down by its `[cleanup]`, and nothing is left listening.
    newgit(&repo, &["tracker", "use", "local-db", "x"]);
    assert!(settles_down(port));
    let removed = newgit(&repo, &["remove", "x"]);
    assert!(removed.contains("cleanup:  db"), "{removed}");
    assert_eq!(calls(&temp), ["up", "down", "up", "down", "down"]);
    assert!(listener_pid(&temp).is_none(), "remove leaked nothing");
    assert!(!answering(port));
}

/// A binding parked without `suspend` — here, one whose stack the drop's
/// `[cleanup]` tore down — is brought back by `prepare` on the same port,
/// and `remove` does not tear it down a second time.
#[test]
fn without_suspend_a_drop_cleans_up_and_use_prepares_on_the_same_ports() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let path = store.paths().resources.join("db.toml");
    let definition = std::fs::read_to_string(&path).expect("read");
    let (head, _) = definition.split_once("[actions.suspend]").expect("suspend");
    let (_, cleanup) = definition.split_once("[cleanup]").expect("cleanup");
    std::fs::write(&path, format!("{head}[cleanup]{cleanup}")).expect("write");

    newgit(&repo, &["spawn", "x", "--use", "branch-db"]);
    let port = db_port(&store);
    let dropped = newgit(&repo, &["tracker", "drop", "branch-db", "x"]);
    assert!(dropped.contains("parked `db`: ran `cleanup`"), "{dropped}");
    assert!(settles_down(port));
    assert_eq!(
        store.find_branch("x").expect("instance").parked["db"].state,
        ParkedState::CleanedUp
    );

    newgit(&repo, &["tracker", "use", "branch-db", "x"]);
    assert_eq!(db_port(&store), port);
    assert!(answering(port));

    newgit(&repo, &["tracker", "drop", "branch-db", "x"]);
    newgit(&repo, &["remove", "x"]);
    assert_eq!(
        calls(&temp),
        ["up", "down", "up", "down"],
        "a parked binding its drop already cleaned up is not cleaned up again"
    );
    assert!(listener_pid(&temp).is_none());
}
