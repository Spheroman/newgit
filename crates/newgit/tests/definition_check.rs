//! A definition that names something that is not there, or still holds a
//! template's placeholder, is caught where the definitions are read:
//! `newgit resource check` fails it, and `spawn` and `status` name it, with
//! the fix, for every instance that binds it. Before, a `[checkpoint]
//! into_tracker` naming no tracker passed every command until the first
//! `checkpoint`, and `EDIT ME` in an install read as a program called
//! `EDIT` missing from `PATH`.

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
    assert!(output.status.success(), "git {args:?} failed");
}

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup() -> (TempDir, Utf8PathBuf, MetadataStore) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-defcheck-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let temp = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
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
    (TempDir(temp), repo, store)
}

fn write_resource(store: &MetadataStore, name: &str, contents: &str) {
    std::fs::write(
        store.paths().resources.join(format!("{name}.toml")),
        contents,
    )
    .expect("write resource");
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

fn check_json(repo: &Utf8Path) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(["resource", "check", "--json"])
        .output()
        .expect("newgit runs");
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}):\n{stdout}"));
    (output.status.code(), value)
}

/// `(name, field, reason)` for every definition problem `resource check`
/// reports.
fn definition_problems(repo: &Utf8Path) -> Vec<(String, String, String)> {
    let (_, value) = check_json(repo);
    value["definitions"]
        .as_array()
        .expect("definitions")
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap_or_default().to_owned(),
                entry["field"].as_str().unwrap_or_default().to_owned(),
                entry["reason"].as_str().expect("reason").to_owned(),
            )
        })
        .collect()
}

const DB: &str = r#"ownership = "branch"

[actions.prepare]
command = "true"

[checkpoint]
mode = "command"
command = "mkdir -p {{snapshot.path}} && echo ok"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "true"
"#;

#[test]
fn a_tracker_named_by_into_tracker_must_exist_before_the_first_checkpoint() {
    let (_temp, repo, store) = setup();
    write_resource(&store, "db", DB);

    let (code, output) = newgit(&repo, &["resource", "check"]);
    assert_eq!(code, Some(1), "{output}");
    assert!(
        output.contains(
            "FAIL  db: [checkpoint] into_tracker — names `db-snapshots`, which is not a \
             tracker: create it with `newgit tracker create db-snapshots`"
        ),
        "{output}"
    );

    // Spawn binds it and says what is wrong, rather than reporting ready.
    let (code, output) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(1), "{output}");
    assert!(output.contains("not ready: `x`"), "{output}");
    assert!(
        output.contains("`newgit tracker create db-snapshots`"),
        "{output}"
    );
    let (_, output) = newgit(&repo, &["status", "x"]);
    assert!(
        output.contains("resource `db`: [checkpoint] into_tracker — names `db-snapshots`"),
        "{output}"
    );

    // The fix it names is the fix.
    let (code, output) = newgit(&repo, &["tracker", "create", "db-snapshots"]);
    assert_eq!(code, Some(0), "{output}");
    let (code, output) = newgit(&repo, &["resource", "check"]);
    assert_eq!(code, Some(0), "{output}");
    let (_, output) = newgit(&repo, &["status", "x"]);
    assert!(!output.contains("not ready"), "{output}");
    let (code, output) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{output}");
    let _ = newgit(&repo, &["remove", "x", "--force"]);
}

#[test]
fn every_name_a_definition_refers_to_is_checked() {
    let (_temp, repo, store) = setup();
    std::fs::write(store.paths().scripts.join("seed.sh"), "#!/bin/sh\n").expect("script");
    write_resource(
        &store,
        "web",
        r#"ownership = "branch"

[ports]
app = { start = 3300, env = "WEB_PORT" }

[exports]
WEB_URL = "http://127.0.0.1:{{ports.app}}/{{exports.NOBODY}}"

[actions.prepare]
command = "{{scripts}}/seed.sh && {{scripts}}/gone.sh --port {{ports.ap}} --url {{exports.WEB_URL}} && echo {{branch.nme}} && docker inspect --format '{{.State.Status}}' c"

[actions.start]
command = "echo {{state_ref}}; exec sleep 100"
long_running = true

[ready]
http = "http://127.0.0.1:{{ports.app}}/"

[checkpoint]
mode = "command"
command = "echo {{exports.WEB_URL}} {{exports.DB_URL}} > {{snapshot.path}}/x"

[restore]
mode = "command"
command = "cat {{state_ref}} {{snapshot.path}}"
"#,
    );
    write_resource(
        &store,
        "db",
        r#"ownership = "branch"

[exports]
DB_URL = "postgres://x"
"#,
    );
    std::fs::write(
        store.paths().trackers.join("notes.toml"),
        "audience = \"user\"\nstorage = \"local\"\nmerge_with_source = false\npaths = []\nresources = [\"ghost\"]\n",
    )
    .expect("tracker");

    let problems = definition_problems(&repo);
    let has = |name: &str, field: &str, needle: &str| {
        problems
            .iter()
            .any(|(n, f, reason)| n == name && f == field && reason.contains(needle))
    };
    assert!(
        has(
            "web",
            "[exports] WEB_URL",
            "`{{exports.NOBODY}}`, which no resource exports"
        ),
        "{problems:#?}"
    );
    assert!(
        has(
            "web",
            "[actions.prepare] command",
            "no .newgit/scripts/gone.sh"
        ),
        "{problems:#?}"
    );
    assert!(
        has(
            "web",
            "[actions.prepare] command",
            "declares no port `ap` (its ports: `app`)"
        ),
        "{problems:#?}"
    );
    assert!(
        has("web", "[actions.prepare] command", "so use `$WEB_URL`"),
        "{problems:#?}"
    );
    assert!(
        has(
            "web",
            "[actions.prepare] command",
            "`{{branch.nme}}`, which is not a template variable"
        ),
        "{problems:#?}"
    );
    assert!(
        has(
            "web",
            "[actions.start] command",
            "`{{state_ref}}`, which has a value only in"
        ),
        "{problems:#?}"
    );
    assert!(
        has(
            "web",
            "[checkpoint] command",
            "a hook sees only its own resource's exports"
        ),
        "{problems:#?}"
    );
    assert!(
        has(
            "web",
            "[restore] command",
            "`{{snapshot.path}}`, which has a value only"
        ),
        "{problems:#?}"
    );
    assert!(
        has(
            "notes",
            "resources",
            "names `ghost`, which is not a resource"
        ),
        "{problems:#?}"
    );
    // The checkpoint has no `into_tracker`, so its `{{snapshot.path}}` has
    // no directory to name.
    assert!(
        problems.iter().any(|(resource, place, problem)| {
            resource == "web"
                && place == "[checkpoint] command"
                && problem.contains("`{{snapshot.path}}`")
                && problem.contains("into_tracker")
        }),
        "{problems:#?}"
    );
    // Nothing else: the script that exists, the ports that are declared,
    // its own export in its checkpoint, `{{state_ref}}` in `[restore]`, and
    // Docker's own `{{.State.Status}}` are all fine.
    assert_eq!(problems.len(), 10, "{problems:#?}");

    // A `depends_on` naming nothing is the graph's to refuse, and the check
    // fails on it too.
    write_resource(
        &store,
        "db",
        "ownership = \"branch\"\ndepends_on = [\"nope\"]\n\n[exports]\nDB_URL = \"postgres://x\"\n",
    );
    let (code, output) = newgit(&repo, &["resource", "check"]);
    assert_eq!(code, Some(1));
    assert!(
        output.contains("FAIL  resource `db` depends on `nope`, which is neither"),
        "{output}"
    );
}

#[test]
fn resource_add_lists_the_placeholders_and_check_names_them_as_placeholders() {
    let (_temp, repo, _store) = setup();
    let (code, output) = newgit(&repo, &["resource", "add", "deps", "--template", "install"]);
    assert_eq!(code, Some(0), "{output}");
    assert!(
        output.contains("edit these lines before spawning"),
        "{output}"
    );
    let listed: Vec<&str> = output
        .lines()
        .filter(|line| {
            line.trim_start()
                .starts_with(".newgit/resources/deps.toml:")
        })
        .collect();
    assert_eq!(listed.len(), 3, "{output}");
    assert!(
        listed.iter().all(|line| line.contains("EDIT ME")),
        "{output}"
    );
    assert!(
        listed.iter().any(|line| line.contains("paths = [")),
        "{output}"
    );
    assert!(
        listed.iter().any(|line| line.contains("produces = [")),
        "{output}"
    );
    assert!(
        listed.iter().any(|line| line.contains("command = ")),
        "{output}"
    );

    let (code, output) = newgit(&repo, &["resource", "check"]);
    assert_eq!(code, Some(1), "{output}");
    for field in [
        "[identity] paths",
        "[identity] produces",
        "[actions.prepare] command",
    ] {
        assert!(
            output.contains(&format!(
                "FAIL  deps: {field} — still the template's placeholder `EDIT ME"
            )),
            "{field}: {output}"
        );
    }
    // Not a program called `EDIT` missing from `PATH`.
    assert!(!output.contains("`EDIT`"), "{output}");

    let (code, output) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(1), "{output}");
    assert!(
        output.contains("resource `deps`: [identity] paths — still the template's placeholder"),
        "{output}"
    );
    let _ = newgit(&repo, &["remove", "x", "--force"]);
}

/// Every bundled template: what `resource add` lists is exactly what
/// `resource check` fails, every failure is a placeholder, and once the
/// placeholders are replaced nothing in any template trips the check.
#[test]
fn every_bundled_template_is_clean_once_its_placeholders_are_replaced() {
    let (_temp, repo, store) = setup();
    let mut listed = 0;
    for template in newgit_core::templates::RESOURCE_TEMPLATES {
        let name = format!("t-{}", template.name);
        let (code, output) = newgit(
            &repo,
            &["resource", "add", &name, "--template", template.name],
        );
        assert_eq!(code, Some(0), "{}: {output}", template.name);
        listed += output
            .lines()
            .filter(|line| {
                line.trim_start()
                    .starts_with(&format!(".newgit/resources/{name}.toml:"))
            })
            .count();
    }

    let problems = definition_problems(&repo);
    assert!(
        problems
            .iter()
            .all(|(_, _, reason)| reason.starts_with("still the template's placeholder")),
        "{problems:#?}"
    );
    assert_eq!(problems.len(), listed, "{problems:#?}");
    assert!(listed > 0);

    for entry in std::fs::read_dir(&store.paths().resources).expect("resources") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(
            &path,
            text.replace("EDIT ME", "edited")
                .replace("EDIT_ME", "edited"),
        )
        .expect("write");
    }
    let problems = definition_problems(&repo);
    assert!(problems.is_empty(), "{problems:#?}");
}
