//! A `tracker use` or `drop` that rewrites a rendered file under a running
//! process says so, and restarts the ones that declare they read it (#249).
//!
//! The case from the issue: `db` is brought by the `branch-db` tracker and
//! renders its port into `apps/admin/.env.local`. `admin`'s dev server was
//! already running when `tracker use branch-db` bound `db` and rewrote the
//! file, and it kept the old port, with nothing in the output to say so.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BindingChoice, BranchManager, StopStartStatus, Uncommitted};
use newgit_core::store::MetadataStore;
use newgit_core::supervisor::Supervisor;

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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

const ENV: &str = "API_URL=http://127.0.0.1:54321\n";

/// `branch-db` brings `db`, which renders into `apps/admin/.env.local`.
/// Three processes autostart regardless: `admin` declares it reads
/// `apps/admin`, `quiet` declares it reads something else, and `worker`
/// declares nothing.
fn setup(temp: &Utf8Path) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("apps/admin")).expect("mkdir");
    std::fs::create_dir_all(repo.join("apps/db")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("apps/admin/.env.local"), ENV).expect("write");
    std::fs::write(repo.join("apps/db/schema.sql"), "create table t ();\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);

    BranchManager::open(store.clone())
        .expect("manager")
        .create_code_tracker("branch-db", "project-devs", &[Utf8PathBuf::from("apps/db")])
        .expect("carve");
    let tracker = store.paths().trackers.join("branch-db.toml");
    let mut definition = std::fs::read_to_string(&tracker).expect("tracker definition");
    definition.push_str("resources = [\"db\"]\n");
    std::fs::write(&tracker, definition).expect("tracker definition");

    let resources = &store.paths().resources;
    std::fs::write(
        resources.join("db.toml"),
        r#"ownership = "branch"

[ports.api]
start = 47311

[[render]]
path = "apps/admin/.env.local"
replace = [{ find = "API_URL=http://127.0.0.1:54321", with = "API_URL=http://127.0.0.1:{{ports.api}}" }]
"#,
    )
    .expect("db");
    for (name, reads, stop) in [
        ("admin", "reads = [\"apps/admin\"]\n", ""),
        ("quiet", "reads = [\"apps/quiet\"]\n", ""),
        ("worker", "", "\n[actions.stop]\nsignal = \"term\"\n"),
    ] {
        std::fs::write(
            resources.join(format!("{name}.toml")),
            format!(
                "ownership = \"workspace\"\n{reads}\n[actions.start]\ncommand = \"sleep 60\"\n\
                 long_running = true\nautostart = true\n{stop}"
            ),
        )
        .expect("resource");
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers and resources"]);

    let manager = BranchManager::open(store).expect("manager");
    manager
        .merge("main", "main")
        .expect("merge the project root");
    manager
}

fn without_db() -> BindingChoice {
    BindingChoice {
        drops: vec!["branch-db".to_owned()],
        ..Default::default()
    }
}

fn pid(manager: &BranchManager, instance: &str, resource: &str) -> Option<u32> {
    let slug = manager
        .store()
        .find_branch(instance)
        .expect("instance")
        .slug;
    Supervisor::new(manager.store().instance_state_dir(&slug)).running_pid(resource)
}

/// Take the instance's processes down, whatever the assertions did.
struct Stop<'a>(&'a BranchManager);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = self.0.stop_instance("x");
    }
}

#[test]
fn a_use_that_rewrites_a_rendered_file_restarts_readers_and_names_the_rest() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager
        .spawn_binding("x", None, &without_db())
        .expect("spawn x");
    let _stop = Stop(&manager);
    let workspace = spawned.branch.workspace_path;
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/admin/.env.local")).expect("env"),
        ENV,
        "precondition: nothing rendered it yet"
    );
    let admin = pid(&manager, "x", "admin").expect("admin autostarted");
    let quiet = pid(&manager, "x", "quiet").expect("quiet autostarted");
    let worker = pid(&manager, "x", "worker").expect("worker autostarted");

    let change = manager
        .use_tracker("x", "branch-db")
        .expect("use branch-db");

    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/admin/.env.local")).expect("env"),
        "API_URL=http://127.0.0.1:47311\n",
        "precondition: binding `db` rendered its port"
    );
    assert_eq!(
        change.stale.changed,
        [Utf8PathBuf::from("apps/admin/.env.local")]
    );

    let restarted: Vec<(&str, &str, StopStartStatus)> = change
        .stale
        .restarted
        .iter()
        .map(|resource| {
            (
                resource.name.as_str(),
                resource.action.as_str(),
                resource.status,
            )
        })
        .collect();
    assert_eq!(
        restarted,
        [("admin", "start", StopStartStatus::Started)],
        "`admin` reads `apps/admin`, so it comes back on the new file"
    );
    let admin_after = pid(&manager, "x", "admin").expect("admin running again");
    assert_ne!(admin_after, admin, "a restart is a new process");

    assert_eq!(
        pid(&manager, "x", "quiet"),
        Some(quiet),
        "`quiet` declared what it reads, and none of it changed"
    );

    assert_eq!(change.stale.may_be_stale.len(), 1, "{:?}", change.stale);
    let stale = &change.stale.may_be_stale[0];
    assert_eq!(
        (stale.name.as_str(), stale.action.as_str()),
        ("worker", "start")
    );
    assert_eq!(
        stale.restart_with,
        "newgit action worker.stop x && newgit action worker.start x"
    );
    assert_eq!(
        pid(&manager, "x", "worker"),
        Some(worker),
        "newgit cannot tell whether `worker` reads it, so says so and leaves it"
    );
}

#[test]
fn a_drop_that_takes_a_rendered_value_out_restarts_its_readers() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    let spawned = manager.spawn("x", None).expect("spawn x");
    let _stop = Stop(&manager);
    let workspace = spawned.branch.workspace_path;
    let admin = pid(&manager, "x", "admin").expect("admin autostarted");
    let quiet = pid(&manager, "x", "quiet").expect("quiet autostarted");

    // Parking `db` takes its port back out of the file, so `admin` would
    // otherwise go on talking to a database that is down.
    let dropped = manager
        .drop_tracker("x", "branch-db", Uncommitted::Refuse)
        .expect("drop branch-db");
    assert_eq!(
        std::fs::read_to_string(workspace.join("apps/admin/.env.local")).expect("env"),
        ENV,
        "back to the committed default"
    );
    assert_eq!(
        dropped.stale.changed,
        [Utf8PathBuf::from("apps/admin/.env.local")]
    );
    assert!(
        dropped
            .stale
            .restarted
            .iter()
            .any(|resource| resource.name == "admin"),
        "{:?}",
        dropped.stale
    );
    assert_ne!(pid(&manager, "x", "admin"), Some(admin), "restarted");
    assert_eq!(
        pid(&manager, "x", "quiet"),
        Some(quiet),
        "`quiet` reads none of it"
    );
}

#[test]
fn reads_outside_the_workspace_is_refused_at_load() {
    let (_guard, temp) = tempdir();
    let path = temp.join("app.toml");
    std::fs::write(
        &path,
        "ownership = \"workspace\"\nreads = [\"../elsewhere/.env\"]\n",
    )
    .expect("write");
    let error = newgit_core::resource::ResourceDefinition::from_file("app", &path)
        .expect_err("`..` refused");
    assert!(error.to_string().contains("workspace-relative"), "{error}");
}
