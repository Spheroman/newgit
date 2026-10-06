//! `{{bound.trackers}}` and `{{bound.paths}}`: a resource's commands see what
//! the instance binds, so an install builds only the bound members.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::{BindingChoice, BranchManager, TrackerMergeAction};
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "ana")
        .env("GIT_AUTHOR_EMAIL", "ana@example.com")
        .env("GIT_COMMITTER_NAME", "ana")
        .env("GIT_COMMITTER_EMAIL", "ana@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn setup(temp: &Utf8Path) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::create_dir_all(repo.join("apps/electron")).expect("mkdir");
    std::fs::write(repo.join("apps/electron/main.js"), "v1\n").expect("write");
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
        .create_code_tracker(
            "electron",
            "project-devs",
            &[Utf8PathBuf::from("apps/electron")],
        )
        .expect("carve");
    let manager = BranchManager::open(store.clone()).expect("reopen");
    manager
        .create_tracker("env", "user", Storage::Local, false)
        .expect("env");
    manager
        .track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "trackers"]);
    let manager = BranchManager::open(store).expect("manager");
    // The project root's own commits reach `main` like anyone's.
    let published = manager
        .merge("main", "main")
        .expect("merge the project root");
    assert!(
        published
            .trackers
            .iter()
            .any(|merge| merge.tracker == "project" && merge.action == TrackerMergeAction::New),
        "{:?}",
        published.trackers
    );
    manager
}

fn spawn(manager: &BranchManager, name: &str) -> Utf8PathBuf {
    manager.spawn(name, None).expect("spawn");
    manager
        .store()
        .find_branch(name)
        .expect("instance")
        .workspace_path
}

#[test]
fn a_prepare_sees_only_the_bound_trackers() {
    let (_guard, temp) = tempdir();
    let manager = setup(&temp);
    std::fs::write(
        manager.store().paths().resources.join("deps.toml"),
        "ownership = \"branch\"\n\n[actions.prepare]\ncommand = \"echo '{{bound.paths}}|{{bound.trackers}}' > bound.txt\"\n",
    )
    .expect("resource");
    let manager = BranchManager::open(manager.store().clone()).expect("reopen");

    let workspace = spawn(&manager, "all");
    let all = std::fs::read_to_string(workspace.join("bound.txt")).expect("read");
    assert_eq!(all.trim(), "apps/electron|electron env");

    manager
        .spawn_binding(
            "some",
            None,
            &BindingChoice {
                uses: Vec::new(),
                drops: vec!["electron".to_owned()],
                ..Default::default()
            },
        )
        .expect("spawn");
    let workspace = manager
        .store()
        .find_branch("some")
        .expect("instance")
        .workspace_path;
    let some = std::fs::read_to_string(workspace.join("bound.txt")).expect("read");
    assert_eq!(some.trim(), "|env");
    assert!(
        !workspace.join("apps/electron").exists(),
        "and it is not checked out"
    );
}
