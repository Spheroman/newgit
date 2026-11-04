//! `suspend` / `resume` under `newgit stop` / `newgit start` (#157): a
//! resource that keeps running without a process newgit supervises — a
//! Supabase stack, a Compose project — is taken down and brought back by an
//! action pair it defines.
//!
//! The resources here stand those in with a witness file each action appends
//! to, so the tests read what ran and in what order.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::{Value, json};

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
/// whatever the instance still runs, so a failed assertion does not leave
/// `sleep`s behind, then removes the directory.
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
    let dir =
        std::env::temp_dir().join(format!("newgit-suspend-resume-{}-{n}", std::process::id()));
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

fn newgit_ok(repo: &Utf8Path, args: &[&str]) {
    let run = newgit(repo, args);
    assert!(
        run.status.success(),
        "newgit {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
}

/// `newgit <verb> x --json`: its exit code and its stdout parsed as one JSON
/// document — which is itself an assertion: nothing may be printed beside it.
fn json_run(repo: &Utf8Path, verb: &str) -> (Option<i32>, Value) {
    let run = newgit(repo, &[verb, "x", "--json"]);
    let json = serde_json::from_str(&run.stdout).unwrap_or_else(|error| {
        panic!(
            "`newgit {verb} x --json` stdout is not one JSON document ({error}):\n{}\nstderr:\n{}",
            run.stdout, run.stderr
        )
    });
    (run.status.code(), json)
}

/// Whether newgit's supervisor has a live process for `resource` of `x`.
fn running(store: &MetadataStore, resource: &str) -> bool {
    let pid_file = store
        .paths()
        .state
        .join("x")
        .join(format!("{resource}.pid"));
    let Some(pid) = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
    else {
        return false;
    };
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

/// A stack `prepare` brings up and newgit cannot see: `suspend` / `resume`
/// append to `witness`, and `resume` fails while `<witness>.fail` exists.
fn stack(witness: &Utf8Path, ownership: &str) -> String {
    format!(
        r#"ownership = "{ownership}"

[actions.prepare]
command = "true"

[actions.suspend]
command = "echo suspend >> {witness}"

[actions.resume]
command = "test ! -e {witness}.fail && echo resume >> {witness}"
"#
    )
}

const WEB: &str = r#"ownership = "branch"
depends_on = ["db"]

[actions.serve]
command = "sleep 1000"
long_running = true
"#;

fn witnessed(witness: &Utf8Path) -> String {
    std::fs::read_to_string(witness).unwrap_or_default()
}

#[test]
fn stop_suspends_after_its_dependents_and_start_resumes_before_them() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("db.log");
    write_resource(&store, "db", &stack(&witness, "branch"));
    write_resource(&store, "web", WEB);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "web.serve", "x"]);

    let (code, stopped) = json_run(&repo, "stop");
    assert_eq!(code, Some(0), "{stopped}");
    assert_eq!(
        stopped,
        json!({
            "instance": "x",
            "resources": [
                {"name": "web", "action": "serve", "status": "stopped"},
                {"name": "db", "action": "suspend", "status": "stopped"},
            ],
        })
    );
    assert_eq!(witnessed(&witness), "suspend\n");

    // Already suspended: a second stop does not suspend it again.
    let (code, again) = json_run(&repo, "stop");
    assert_eq!(code, Some(0), "{again}");
    assert_eq!(again["resources"], json!([]));
    assert_eq!(witnessed(&witness), "suspend\n");

    let (code, started) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(
        started,
        json!({
            "instance": "x",
            "resources": [
                {"name": "db", "action": "resume", "status": "started"},
                {"name": "web", "action": "serve", "status": "started"},
            ],
        })
    );
    assert_eq!(witnessed(&witness), "suspend\nresume\n");
    assert!(running(&store, "web"));

    let (code, again) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{again}");
    assert_eq!(again["resources"], json!([]));
}

/// A resource can be both: a stack, and a process that talks to it. The
/// process goes down first and comes back last.
#[test]
fn a_resource_with_a_process_and_a_suspend_owes_both() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("db.log");
    write_resource(
        &store,
        "db",
        &format!(
            "{}\n[actions.functions]\ncommand = \"sleep 1000\"\nlong_running = true\n",
            stack(&witness, "branch")
        ),
    );
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "db.functions", "x"]);

    let (_, stopped) = json_run(&repo, "stop");
    assert_eq!(
        stopped["resources"],
        json!([
            {"name": "db", "action": "functions", "status": "stopped"},
            {"name": "db", "action": "suspend", "status": "stopped"},
        ])
    );
    // Its own `resume` failing holds its process back too.
    std::fs::write(format!("{witness}.fail"), "").expect("arm failure");
    let (code, failed) = json_run(&repo, "start");
    assert_eq!(code, Some(1), "{failed}");
    assert_eq!(failed["resources"][1]["action"], "functions");
    assert_eq!(failed["resources"][1]["status"], "failed");
    assert!(!running(&store, "db"));

    std::fs::remove_file(format!("{witness}.fail")).expect("disarm");
    let (code, started) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(
        started["resources"],
        json!([
            {"name": "db", "action": "resume", "status": "started"},
            {"name": "db", "action": "functions", "status": "started"},
        ])
    );
    assert!(running(&store, "db"));
}

/// The case that matters to T3 Code: the stack does not come back, so the
/// dev server is not started against it, and both are owed to the next
/// `start`.
#[test]
fn a_failed_resume_holds_back_its_dependents_and_is_retried() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("db.log");
    write_resource(&store, "db", &stack(&witness, "branch"));
    write_resource(&store, "web", WEB);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "web.serve", "x"]);
    newgit_ok(&repo, &["stop", "x"]);

    std::fs::write(format!("{witness}.fail"), "").expect("arm failure");
    let (code, started) = json_run(&repo, "start");
    assert_eq!(code, Some(1), "{started}");
    let resources = started["resources"].as_array().expect("resources");
    assert_eq!(resources.len(), 2, "{started}");
    assert_eq!(
        (
            &resources[0]["name"],
            &resources[0]["action"],
            &resources[0]["status"]
        ),
        (&json!("db"), &json!("resume"), &json!("failed"))
    );
    assert!(
        resources[0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("exited 1") && error.contains("db.resume")),
        "names the exit and the log: {started}"
    );
    assert_eq!(resources[1]["name"], "web");
    assert_eq!(resources[1]["status"], "failed");
    assert!(
        resources[1]["error"]
            .as_str()
            .is_some_and(|error| error.contains("`db`")),
        "names the dependency: {started}"
    );
    assert!(!running(&store, "web"));

    std::fs::remove_file(format!("{witness}.fail")).expect("disarm");
    let (code, retried) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{retried}");
    assert_eq!(
        retried["resources"],
        json!([
            {"name": "db", "action": "resume", "status": "started"},
            {"name": "web", "action": "serve", "status": "started"},
        ])
    );
}

/// A failed `suspend` leaves the resource up as far as newgit knows: there
/// is nothing for `start` to resume, and the next `stop` tries again.
#[test]
fn a_failed_suspend_is_not_recorded_and_the_next_stop_retries_it() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let witness = temp.join("db.log");
    write_resource(
        &store,
        "db",
        &stack(&witness, "branch").replace(
            "command = \"echo suspend",
            &format!("command = \"test ! -e {witness}.fail && echo suspend"),
        ),
    );
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);

    std::fs::write(format!("{witness}.fail"), "").expect("arm failure");
    let (code, stopped) = json_run(&repo, "stop");
    assert_eq!(code, Some(1), "{stopped}");
    assert_eq!(stopped["resources"][0]["action"], "suspend");
    assert_eq!(stopped["resources"][0]["status"], "failed");
    let (code, started) = json_run(&repo, "start");
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(started["resources"], json!([]));

    std::fs::remove_file(format!("{witness}.fail")).expect("disarm");
    let (code, stopped) = json_run(&repo, "stop");
    assert_eq!(code, Some(0), "{stopped}");
    assert_eq!(
        stopped["resources"],
        json!([{"name": "db", "action": "suspend", "status": "stopped"}])
    );
}

/// Shared with every other instance, a `project` resource is not this
/// instance's to suspend. Nor is one whose `prepare` failed: it never came
/// up.
#[test]
fn shared_and_never_ready_resources_are_not_suspended() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let shared = temp.join("shared.log");
    let broken = temp.join("broken.log");
    write_resource(&store, "shared", &stack(&shared, "project"));
    write_resource(
        &store,
        "broken",
        &stack(&broken, "branch").replace("command = \"true\"", "command = \"exit 3\""),
    );
    let repo = store.paths().project_root.clone();
    // Exits 1 for `broken`'s prepare; the instance is spawned regardless.
    let _ = newgit(&repo, &["spawn", "x"]);

    let (code, stopped) = json_run(&repo, "stop");
    assert_eq!(code, Some(0), "{stopped}");
    assert_eq!(stopped["resources"], json!([]));
    assert_eq!(witnessed(&shared), "");
    assert_eq!(witnessed(&broken), "");
}

#[test]
fn suspend_and_resume_come_as_a_pair_of_commands_that_finish() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    for (contents, expect) in [
        (
            "[actions.suspend]\ncommand = \"true\"\n",
            "`suspend` has no `resume`",
        ),
        (
            "[actions.resume]\ncommand = \"true\"\n",
            "`resume` has no `suspend`",
        ),
        (
            "[actions.suspend]\ncommand = \"true\"\n\n[actions.resume]\ncommand = \"sleep 1\"\nlong_running = true\n",
            "cannot be long_running",
        ),
    ] {
        write_resource(
            &store,
            "db",
            &format!("ownership = \"branch\"\n\n{contents}"),
        );
        let run = newgit(&repo, &["status"]);
        assert!(!run.status.success(), "accepted:\n{contents}");
        assert!(run.stderr.contains(expect), "{expect}:\n{}", run.stderr);
    }
}
