//! `gitignore_warnings` asks Git about every tracker path at once: each path
//! still gets the answer it would get asked alone.

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

#[test]
fn each_tracker_path_is_judged_on_its_own() {
    let temp = tempfile::tempdir().expect("tempdir");
    let temp = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("docs")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    std::fs::write(repo.join("docs/guide.md"), "guide\n").expect("write");
    std::fs::write(repo.join(".gitignore"), ".env.local\nsecrets\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");

    // Ignored (`.env.local`, `secrets/`), not ignored (`cache/`, `READ`,
    // which only shares a prefix with the committed README.md), and
    // committed (`docs/`, a directory Git tracks a file under).
    std::fs::write(
        store.paths().trackers.join("mixed.toml"),
        "audience = \"project-devs\"\nstorage = \"local\"\n\
         paths = [\".env.local\", \"cache/\", \"docs/\"]\n",
    )
    .expect("write def");
    std::fs::write(
        store.paths().trackers.join("other.toml"),
        "audience = \"project-devs\"\nstorage = \"local\"\npaths = [\"secrets/\", \"READ\"]\n",
    )
    .expect("write def");

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("manager");
    let mut warnings = manager.gitignore_warnings();
    warnings.sort();
    let about = |path: &str| {
        warnings
            .iter()
            .filter(|warning| warning.contains(&format!("`{path}`")))
            .collect::<Vec<_>>()
    };
    assert!(about(".env.local").is_empty(), "{warnings:#?}");
    assert!(about("secrets").is_empty(), "{warnings:#?}");
    let cache = about("cache");
    assert_eq!(cache.len(), 1, "{warnings:#?}");
    assert!(cache[0].contains("does not gitignore"), "{}", cache[0]);
    let read = about("READ");
    assert_eq!(read.len(), 1, "{warnings:#?}");
    assert!(read[0].contains("does not gitignore"), "{}", read[0]);
    let docs = about("docs");
    assert_eq!(docs.len(), 1, "{warnings:#?}");
    assert!(docs[0].contains("dual-tracked"), "{}", docs[0]);
    assert_eq!(warnings.len(), 3, "{warnings:#?}");
}
