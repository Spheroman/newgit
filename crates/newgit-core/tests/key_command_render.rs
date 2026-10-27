//! `[identity] key_command` is rendered like an action `command` before it
//! runs. The reference says every command is rendered, and a `key_command`
//! has the same use for `{{bound.paths}}` as the install it keys: an
//! argument naming what to inspect. It used to be handed to the shell
//! verbatim, so `{{branch.name}}` printed as `{{branch.name}}`.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
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

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A project with one code tracker, `app`, owning `apps/a`, so the
/// instance's `{{bound.*}}` has something to render.
fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("apps/a")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("lock.txt"), "v1\n").expect("write lock");
    std::fs::write(repo.join("apps/a/index.js"), "a\n").expect("write");
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
        .create_code_tracker("app", "project-devs", &[Utf8PathBuf::from("apps/a")])
        .expect("carve");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    // The project root's own commits reach `main` like anyone's.
    BranchManager::open(store.clone())
        .expect("manager")
        .merge("main", "main")
        .expect("merge the project root");
    store
}

#[test]
fn key_command_is_rendered_before_it_runs() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let seen = temp.join("key-command-saw.txt");

    std::fs::write(
        store.paths().resources.join("deps.toml"),
        format!(
            r#"ownership = "workspace"

[ports]
app = {{ start = 4300 }}

[identity]
paths = ["lock.txt"]
produces = ["node_modules"]
key_command = "printf '%s|%s|%s|%s|%s|%s' '{{{{branch.name}}}}' '{{{{workspace}}}}' '{{{{scripts}}}}' '{{{{ports.app}}}}' '{{{{bound.trackers}}}}' '{{{{bound.paths}}}}' > {seen}; echo v24"

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/marker"
"#
        ),
    )
    .expect("write resource");
    let manager = BranchManager::open(MetadataStore::at(&repo)).expect("manager");

    let outcome = manager.spawn("one", None).expect("spawn one");
    let branch = outcome.branch.clone();
    let port = branch
        .resources
        .get("deps")
        .and_then(|binding| binding.resolved_ports.get("app"))
        .copied()
        .expect("deps has an app port");

    let saw = std::fs::read_to_string(&seen).expect("the key_command ran");
    assert_eq!(
        saw,
        format!(
            "one|{}|{}|{port}|app|apps/a",
            branch.workspace_path,
            store.paths().scripts
        ),
        "key_command sees what an action command sees"
    );
}
