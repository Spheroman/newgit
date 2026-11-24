//! #85: an instance bound before its resource definition changed. `status`
//! names each way the binding differs from the definition and the commands
//! that apply it; `resource rebind` applies it without moving a port; what
//! newgit reads live is not flagged; and a definition deleted by hand leaves
//! no process behind `stop` or `remove`.

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
    assert!(output.status.success(), "git {args:?} failed");
}

struct Project {
    temp: Utf8PathBuf,
    repo: Utf8PathBuf,
    store: MetadataStore,
}

impl Drop for Project {
    fn drop(&mut self) {
        // Whatever a test left running goes with it.
        for name in ["x", "y"] {
            let _ = newgit(&self.repo, &["stop", name]);
        }
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}

impl Project {
    fn new(label: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("newgit-stale-def-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let temp =
            Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize")).expect("utf8");
        let repo = temp.join("store");
        std::fs::create_dir_all(&repo).expect("mkdir");
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("config.json"), "{\"port\": 3000}\n").expect("write");
        std::fs::write(repo.join("other.txt"), "a 3000 b\n").expect("write");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
        let mut config = store.load_config().expect("config");
        config.workspace = Some(WorkspaceSection {
            root: Some(temp.join("workspaces")),
            materializer: None,
        });
        store.write_config(&config).expect("write config");
        Self { temp, repo, store }
    }

    fn define(&self, name: &str, toml: &str) {
        std::fs::write(self.definition(name), toml).expect("write definition");
    }

    fn definition(&self, name: &str) -> Utf8PathBuf {
        self.store.paths().resources.join(format!("{name}.toml"))
    }

    fn run(&self, args: &[&str]) -> String {
        let (ok, out) = newgit(&self.repo, args);
        assert!(ok, "newgit {args:?} failed:\n{out}");
        out
    }

    fn status(&self, instance: &str) -> String {
        newgit(&self.repo, &["status", instance]).1
    }
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The number after `(pid ` in `text`.
fn pid_in(text: &str) -> u32 {
    let start = text.find("(pid ").expect("a pid is named") + "(pid ".len();
    text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("pid")
}

const STACK: &str = r#"ownership = "branch"

[ports]
api = { start = 55310 }

[exports]
API_URL = "http://127.0.0.1:{{ports.api}}"

[[render]]
path = "config.json"
replace = [{ find = "3000", with = "{{ports.api}}" }]

[actions.prepare]
command = "echo one >> prepared.txt"
"#;

#[test]
fn each_edit_a_rebind_applies_is_named_until_the_rebind_runs() {
    let project = Project::new("bind");
    project.define("stack", STACK);
    project.run(&["spawn", "x"]);
    let api = project.store.find_branch("x").expect("x").resources["stack"].resolved_ports["api"];
    assert!(
        !project.status("x").contains("was bound from an older"),
        "{}",
        project.status("x")
    );

    let edited = STACK
            .replace(
                "api = { start = 55310 }",
                "api = { start = 55310 }\nadmin = { start = 55320 }",
            )
            .replace("127.0.0.1", "localhost")
            .replace(
                "[exports]\n",
                "[exports]\nADMIN_URL = \"http://127.0.0.1:{{ports.admin}}\"\n",
            )
            .replace(
                "[actions.prepare]",
                "[[render]]\npath = \"other.txt\"\nreplace = [{ find = \"3000\", with = \"{{ports.api}}\" }]\n\n[actions.prepare]",
            )
            .replace("echo one", "echo two");
    project.define("stack", &edited);
    let status = project.status("x");
    let definition = project.definition("stack");
    for reason in [
        format!("`stack` was bound from an older {definition}: "),
        "port `admin` declared since, so nothing claims it for this instance".to_owned(),
        "export `ADMIN_URL` declared since".to_owned(),
        "export `API_URL` changed".to_owned(),
        "`[[render]]` of other.txt added".to_owned(),
        "`prepare` changed since it ran here".to_owned(),
        "apply it with `newgit resource rebind stack x`".to_owned(),
    ] {
        assert!(status.contains(&reason), "missing {reason:?} in:\n{status}");
    }
    assert!(status.contains("not ready"), "{status}");

    project.run(&["resource", "rebind", "stack", "x"]);
    let status = project.status("x");
    assert!(!status.contains("was bound from an older"), "{status}");
    let binding = &project.store.find_branch("x").expect("x").resources["stack"];
    // The port it had stays; only the one declared since is allocated.
    assert_eq!(binding.resolved_ports["api"], api);
    assert!(binding.resolved_ports.contains_key("admin"));
    assert_eq!(
        binding.resolved_exports["API_URL"],
        format!("http://localhost:{api}")
    );

    // What the definition no longer declares is named as still held.
    project.define(
        "stack",
        &edited.replace("[exports]\n", "[exports]\nOLD = \"x\"\n"),
    );
    project.run(&["resource", "rebind", "stack", "x"]);
    project.define("stack", STACK);
    let status = project.status("x");
    for reason in [
        "port `admin` no longer declared, still claimed here",
        "exports `ADMIN_URL`, `OLD` no longer declared, still in every command's environment",
        "`[[render]]` of other.txt removed, its values still in the file",
    ] {
        assert!(status.contains(reason), "missing {reason:?} in:\n{status}");
    }
    project.run(&["resource", "rebind", "stack", "x"]);
    assert!(!project.status("x").contains("was bound from an older"));
}

#[test]
fn what_newgit_reads_live_is_not_flagged() {
    let project = Project::new("live");
    project.define("db", "ownership = \"branch\"\n");
    project.define("stack", STACK);
    project.run(&["spawn", "x"]);

    project.define(
        "stack",
        &format!(
            "# a comment\ndepends_on = [\"db\"]\n{}\n[ready]\ntimeout_seconds = 9\n",
            STACK.replace("start = 55310", "start = 55400")
        ),
    );
    let status = project.status("x");
    assert!(!status.contains("was bound from an older"), "{status}");
    assert!(!status.contains("not ready"), "{status}");
}

const PROC: &str = r#"ownership = "branch"

[actions.prepare]
command = "true"

[actions.start]
command = "sleep 300"
long_running = true
autostart = true
"#;

#[test]
fn a_running_process_is_named_with_what_restarts_it() {
    let project = Project::new("proc");
    project.define("proc", PROC);
    project.run(&["spawn", "x"]);
    assert!(!project.status("x").contains("not ready"));

    // Only its command changed: a restart takes it up.
    project.define("proc", &PROC.replace("sleep 300", "sleep 301"));
    let status = project.status("x");
    assert!(
        status.contains(&format!(
            "`proc.start` is running a command {} no longer gives — restart it with `newgit \
             stop x && newgit start x`",
            project.definition("proc")
        )),
        "{status}"
    );
    project.run(&["stop", "x"]);
    project.run(&["start", "x"]);
    assert!(!project.status("x").contains("not ready"));

    // A change a rebind applies, while it runs: `rebind` refuses a running
    // process, so the line names the stop and the start around it.
    project.define(
        "proc",
        &PROC
            .replace("sleep 300", "sleep 301")
            .replace("command = \"true\"", "command = \":\""),
    );
    let status = project.status("x");
    assert!(
        status.contains(
            "apply it with `newgit stop x && newgit resource rebind proc x && newgit start x`"
        ),
        "{status}"
    );
    project.run(&["stop", "x"]);
    project.run(&["resource", "rebind", "proc", "x"]);
    project.run(&["start", "x"]);
    let status = project.status("x");
    assert!(!status.contains("not ready"), "{status}");
}

#[test]
fn a_definition_deleted_by_hand_leaves_no_process_behind() {
    let project = Project::new("deleted");
    project.define("proc", PROC);
    project.define(
        "stack",
        "ownership = \"branch\"\n\n[ports]\napi = { start = 55330 }\n\n[[render]]\npath = \"config.json\"\nreplace = [{ find = \"3000\", with = \"{{ports.api}}\" }]\n",
    );
    project.run(&["spawn", "x"]);
    project.run(&["spawn", "y"]);
    let workspace = project.store.find_branch("x").expect("x").workspace_path;
    std::fs::remove_file(project.definition("proc")).expect("rm");
    std::fs::remove_file(project.definition("stack")).expect("rm");

    let status = project.status("x");
    let pid = pid_in(&status);
    assert!(alive(pid));
    assert!(
        status.contains(&format!(
            "`proc` is bound here, but {} is gone and its process (pid {pid}) is still running \
             — stop it with `newgit stop x`, then release its binding with `newgit resource \
             rebind proc x`",
            project.definition("proc")
        )),
        "{status}"
    );
    assert!(
        status.contains("is gone and it still claims port `api` — release its binding with `newgit resource rebind stack x`"),
        "{status}"
    );

    let (ok, out) = newgit(&project.repo, &["resource", "rebind", "proc", "x"]);
    assert!(
        !ok && out.contains("stop it first with `newgit stop x`"),
        "{out}"
    );
    project.run(&["stop", "x"]);
    assert!(!alive(pid), "`stop` left the process running");

    let out = project.run(&["resource", "rebind", "stack", "x"]);
    assert!(
        out.contains("`[cleanup]` went with its definition"),
        "{out}"
    );
    assert!(out.contains("api="), "{out}");
    project.run(&["resource", "rebind", "proc", "x"]);
    let branch = project.store.find_branch("x").expect("x");
    assert!(branch.resources.is_empty(), "{:?}", branch.resources);
    // Its values came out of the file it rendered.
    assert_eq!(
        std::fs::read_to_string(workspace.join("config.json")).expect("read"),
        "{\"port\": 3000}\n"
    );
    assert!(!project.status("x").contains("not ready"));

    // `remove` stops it too, rather than leaving it holding what it holds.
    let pid = pid_in(&project.status("y"));
    assert!(alive(pid));
    project.run(&["remove", "y", "--purge", "--force"]);
    assert!(!alive(pid), "`remove` left the process running");
}

#[test]
fn a_definition_added_after_spawn_is_named_until_bound() {
    let project = Project::new("added");
    project.define("stack", STACK);
    project.run(&["spawn", "x"]);
    project.define(
        "extra",
        "ownership = \"branch\"\n\n[ports]\nn = { start = 55340 }\n",
    );

    let status = project.status("x");
    assert!(
        status.contains(&format!(
            "`extra` is defined in {} but not bound here — bind it with `newgit resource rebind \
             extra x`",
            project.definition("extra")
        )),
        "{status}"
    );
    let (_, json) = newgit(&project.repo, &["status", "x", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&json).expect("json");
    let instance = &json["instances"][0];
    assert_eq!(instance["name"], "x");
    assert_eq!(instance["status"], "not ready");
    assert!(
        instance["problems"][0]
            .as_str()
            .is_some_and(|problem| problem.contains("`extra` is defined")),
        "{json}"
    );

    project.run(&["resource", "rebind", "extra", "x"]);
    let (_, json) = newgit(&project.repo, &["status", "x", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(json["instances"][0]["status"], "ready", "{json}");
    assert_eq!(json["instances"][0]["problems"], serde_json::json!([]));
}
