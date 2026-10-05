//! `newgit env` and `newgit action --dry-run` (#80): seeing what a hook will
//! run with, and what it will run, without running it.
//!
//! The property that matters is that the answer is *exactly* what a hook
//! gets — a printed environment that drifts from the real one is worse than
//! none, because someone will trust it before a destructive command. So the
//! core test compares `newgit env` against the environment a real action
//! dumped, value for value.

use std::collections::BTreeMap;
use std::process::{Command, Output};

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
    let dir = std::env::temp_dir().join(format!("newgit-env-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

/// `db` publishes a port, exports (one needing shell quoting), and a value
/// only an action's stdout can supply; `app` depends on it, so the layering
/// across resources is exercised too.
const DB_RESOURCE: &str = r#"ownership = "branch"

[ports]
pg = { start = 4720, env = "DB_PORT" }

[exports]
DB_URL = "postgres://u:p@127.0.0.1:{{ports.pg}}/{{branch.slug}}"
DB_NOTE = "it's got $pace & quotes"

[actions.prepare]
command = "true"

[actions.mint]
command = "echo DB_TOKEN=tok_123"
captures = ["DB_TOKEN"]

[actions.dump]
command = "env > hook-env.txt"

[actions.connect]
command = "psql -p {{ports.pg}} -d {{branch.slug}} -c '{{nope}}'"

[actions.serve]
command = "sleep 30"
long_running = true

[actions.stop]
signal = "term"
"#;

const APP_RESOURCE: &str = r#"ownership = "branch"
depends_on = ["db"]

[exports]
APP_NAME = "app-{{branch.slug}}"
"APP-LABEL" = "label"
"#;

/// A store with both resources and one spawned instance, `feature-a`.
fn setup(temp: &Utf8Path) -> Utf8PathBuf {
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
    for (name, contents) in [("db", DB_RESOURCE), ("app", APP_RESOURCE)] {
        std::fs::write(
            store.paths().resources.join(format!("{name}.toml")),
            contents,
        )
        .expect("write resource");
    }

    newgit_ok(&repo, &["spawn", "feature-a"]);
    repo
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs")
}

fn newgit_ok(repo: &Utf8Path, args: &[&str]) -> String {
    let output = newgit(repo, args);
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn workspace(repo: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(
        newgit_ok(repo, &["status", "feature-a", "--path"])
            .trim()
            .to_owned(),
    )
}

/// `KEY=VALUE` lines — `env`'s output — as a map. Values here are single-line.
fn parse_env(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// The declaration comment `newgit env` printed for a variable.
fn source_of<'a>(printed: &'a str, name: &str) -> &'a str {
    printed
        .lines()
        .find(|line| {
            line.starts_with(&format!("export {name}=")) || line.starts_with(&format!("# {name}="))
        })
        .unwrap_or_else(|| panic!("`{name}` not printed:\n{printed}"))
        .rsplit_once("  # ")
        .expect("every line names its source")
        .1
}

#[test]
fn env_prints_exactly_what_a_hook_receives() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    newgit_ok(&repo, &["action", "db.mint", "feature-a"]);
    newgit_ok(&repo, &["action", "db.dump", "feature-a"]);
    let hook = parse_env(
        &std::fs::read_to_string(workspace(&repo).join("hook-env.txt")).expect("hook dumped env"),
    );

    let printed = newgit_ok(&repo, &["env", "feature-a"]);
    // Sourcing the output as printed must reproduce every value — which
    // proves the quoting and the values in one comparison.
    let sourced = Command::new("sh")
        .args(["-c", "eval \"$1\"; env", "sh", &printed])
        .env_clear()
        .output()
        .expect("sh runs");
    assert!(sourced.status.success(), "output is not valid shell");
    let sourced = parse_env(&String::from_utf8(sourced.stdout).expect("utf8"));

    // Every assignable name is `export`ed, so a plain `eval` reaches the
    // programs the shell starts, not just the shell itself.
    let names: Vec<&str> = printed
        .lines()
        .filter(|line| !line.starts_with("# "))
        .map(|line| {
            line.strip_prefix("export ")
                .expect("every assignable variable is exported")
                .split_once('=')
                .expect("NAME=value")
                .0
        })
        .collect();
    for name in [
        "DB_PORT",
        "DB_URL",
        "DB_NOTE",
        "DB_TOKEN",
        "APP_NAME",
        "NEWGIT_BRANCH",
        "NEWGIT_WORKSPACE",
    ] {
        assert!(names.contains(&name), "`{name}` missing:\n{printed}");
    }
    for name in names {
        assert_eq!(
            sourced.get(name),
            hook.get(name),
            "`{name}` printed differently from what the hook got:\n{printed}"
        );
    }
    assert_eq!(hook["DB_NOTE"], "it's got $pace & quotes");

    // A name no shell can assign still reaches the hook; `env` shows it
    // commented out, so the output stays valid shell instead of `eval`
    // running `APP-LABEL=label` as a command.
    assert_eq!(hook["APP-LABEL"], "label");
    assert!(
        printed
            .lines()
            .any(|line| line.starts_with("# APP-LABEL=label")),
        "unassignable name not shown commented out:\n{printed}"
    );
    assert!(source_of(&printed, "APP-LABEL").contains("a shell cannot assign this name"));
    assert!(!sourced.contains_key("APP-LABEL"));
}

#[test]
fn env_names_the_declaration_behind_every_variable() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    newgit_ok(&repo, &["action", "db.mint", "feature-a"]);
    let printed = newgit_ok(&repo, &["env", "feature-a"]);

    assert_eq!(source_of(&printed, "DB_PORT"), "db [ports.pg]");
    assert_eq!(source_of(&printed, "DB_URL"), "db [exports]");
    assert_eq!(
        source_of(&printed, "DB_TOKEN"),
        "db [actions.mint] captures"
    );
    assert_eq!(source_of(&printed, "APP_NAME"), "app [exports]");
    assert_eq!(source_of(&printed, "NEWGIT_BRANCH"), "newgit");
}

#[test]
fn env_is_inferred_inside_a_workspace() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let printed = newgit_ok(&workspace(&repo), &["env"]);
    assert!(
        printed.contains("NEWGIT_BRANCH=feature-a"),
        "no instance inferred:\n{printed}"
    );
}

#[test]
fn dry_run_prints_the_rendered_command_and_runs_nothing() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let workspace = workspace(&repo);
    let port = parse_env(&newgit_ok(&repo, &["env", "feature-a"]))["export DB_PORT"]
        .split_whitespace()
        .next()
        .expect("port")
        .to_owned();

    let out = newgit_ok(&repo, &["action", "db.dump", "feature-a", "--dry-run"]);
    assert!(out.contains("would run"), "{out}");
    assert!(out.contains(&format!("in:       {workspace}")), "{out}");
    assert!(out.contains("command:  env > hook-env.txt"), "{out}");
    assert!(
        !workspace.join("hook-env.txt").exists(),
        "a dry run ran the command"
    );

    let out = newgit_ok(&repo, &["action", "db.connect", "feature-a", "--dry-run"]);
    assert!(
        out.contains(&format!(
            "command:  psql -p {port} -d feature-a -c '{{{{nope}}}}'"
        )),
        "template not resolved:\n{out}"
    );
    assert!(out.contains("{{nope}} is not a variable"), "{out}");

    let out = newgit_ok(&repo, &["action", "db.mint", "feature-a", "--dry-run"]);
    assert!(out.contains("captures: DB_TOKEN"), "{out}");

    let out = newgit_ok(&repo, &["action", "db.serve", "feature-a", "--dry-run"]);
    assert!(out.contains("would start, supervised"), "{out}");

    let out = newgit_ok(&repo, &["action", "db.stop", "feature-a", "--dry-run"]);
    assert!(out.contains("would send"), "{out}");
}

#[test]
fn dry_run_refuses_what_a_real_run_refuses() {
    let (_guard, temp) = tempdir();
    let repo = setup(&temp);
    let output = newgit(&repo, &["action", "db.nope", "feature-a", "--dry-run"]);
    assert!(!output.status.success(), "unknown action accepted");
}
