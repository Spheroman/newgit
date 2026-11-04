//! `newgit tracker migrate` (docs/design/trackers.md, *Migration*): a project
//! laid out before mounts — `project` holding `.newgit/`, `access` unmounted,
//! the old notes naming each view commit's tracker commits — moved onto
//! pins, gated on recomposing every branch and checkpoint in a fresh
//! repository to the same ids.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::compose::{self, ComposeOptions};
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

fn git_env(dir: &Utf8Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "ana")
        .env("GIT_AUTHOR_EMAIL", "ana@example.com")
        .env("GIT_COMMITTER_NAME", "ana")
        .env("GIT_COMMITTER_EMAIL", "ana@example.com")
        .envs(env.iter().copied())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    git_env(dir, args, &[])
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

const OLD_ROOT: &str = "audience = \"project-devs\"\nstorage = \"local\"\nmerge_with_source = true\npaths = [\".\"]\ngit = true\n";
const OLD_PROJECT: &str = "audience = \"project-devs\"\nstorage = \"local\"\nmerge_with_source = true\npaths = [\".newgit\"]\ngit = true\n";
const OLD_ACCESS: &str = "audience = \"project-devs\"\nstorage = \"local\"\nmerge_with_source = true\npaths = [\".newgit/trackers\"]\ngit = true\nmaintainers = \"owner\"\n";

/// A project as the old engine left it: its history with `.newgit/`
/// committed the old way, a signed commit among it, the tracker store's
/// branches, and the notes saying which tracker commits each view commit
/// stood for. Returns the store, `main`'s view, and an instance's.
fn old_project(temp: &Utf8Path) -> (MetadataStore, String, String) {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let trackers = store.paths().trackers.clone();
    std::fs::write(trackers.join("root.toml"), OLD_ROOT).expect("write");
    std::fs::write(trackers.join("project.toml"), OLD_PROJECT).expect("write");
    std::fs::write(trackers.join("access.toml"), OLD_ACCESS).expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "newgit"]);

    // A commit as a forge signs it, made byte for byte.
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let raw = format!(
        "tree {tree}\nparent {head}\nauthor GitHub <noreply@github.com> 5000 +0000\ncommitter GitHub <noreply@github.com> 5000 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n wsFcBAABCAAQ\n -----END PGP SIGNATURE-----\n\nMerge pull request #1"
    );
    let path = temp.join("signed");
    std::fs::write(&path, raw).expect("write");
    let signed = git(
        &repo,
        &[
            "hash-object",
            "--literally",
            "-t",
            "commit",
            "-w",
            path.as_str(),
        ],
    );
    git(&repo, &["reset", "-q", "--hard", &signed]);
    let main = signed.clone();

    // An instance's work, on a branch of its own.
    git(&repo, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(repo.join("feature.txt"), "work\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "feature work"]);
    let feature = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);

    // The old store: one branch per tracker, and the notes. (The old
    // trackers' own commits do not matter to the migration, only which view
    // commit each state was.)
    let old = store.paths().tracker_store.clone();
    let store_dir = format!("--git-dir={old}");
    for tracker in ["root", "project", "access"] {
        git_env(
            &repo,
            &[
                "push",
                "-q",
                old.as_str(),
                &format!("{head}:refs/trackers/{tracker}/main"),
            ],
            &[],
        );
    }
    git(
        &repo,
        &[
            "push",
            "-q",
            "-f",
            old.as_str(),
            &format!("{main}:refs/trackers/root/main"),
            &format!("{feature}:refs/trackers/root/instances/feature"),
        ],
    );
    let _ = &store_dir;
    let note = |view: &str, root: &str| {
        let text = format!("access {head}\nproject {head}\nroot {root}\n");
        git(
            &repo,
            &[
                "notes",
                "--ref",
                compose::NOTES_REF,
                "add",
                "-f",
                "-m",
                &text,
                view,
            ],
        );
    };
    note(&main, &main);
    note(&feature, &feature);
    (store, main, feature)
}

fn root_main(store: &MetadataStore) -> String {
    git(
        &store.paths().project_root,
        &[
            &format!("--git-dir={}", store.paths().tracker_store),
            "rev-parse",
            "refs/trackers/root/main",
        ],
    )
}

#[test]
fn a_dry_run_runs_the_gate_and_moves_nothing() {
    let (_guard, temp) = tempdir();
    let (store, main, _) = old_project(&temp);
    let repo = store.paths().project_root.clone();
    let manager = BranchManager::open(store.clone()).expect("manager");
    assert!(manager.legacy_layout());
    let refused = manager.spawn("x", None).expect_err("old layout");
    assert!(
        refused.to_string().contains("newgit tracker migrate"),
        "{refused}"
    );

    let outcome = manager.migrate(true).expect("dry run");
    let text = outcome.lines.join("\n");
    assert!(text.contains("Gate: recomposed"), "{text}");
    assert!(text.contains("Dry run: nothing moved"), "{text}");
    assert_eq!(outcome.checked, 2, "main and the instance: {text}");
    assert_eq!(root_main(&store), main, "the store did not move");
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        main,
        "the project root did not move"
    );
    assert!(store.paths().trackers.join("project.toml").is_file());
    assert!(
        git(
            &repo,
            &["notes", "--ref", compose::NOTES_REF, "show", &main]
        )
        .contains("project"),
        "the old notes are back"
    );
    // And a real run after it still finds everything.
    assert!(manager.migrate(false).is_ok());
}

#[test]
fn the_migration_folds_project_into_root_mounts_access_and_keeps_every_id() {
    let (_guard, temp) = tempdir();
    let (store, main, feature) = old_project(&temp);
    let repo = store.paths().project_root.clone();
    let outcome = BranchManager::open(store.clone())
        .expect("manager")
        .migrate(false)
        .expect("migrate");
    let text = outcome.lines.join("\n");

    // One commit on top of `main`'s history, which is root's, commit for commit.
    let head = git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(git(&repo, &["rev-parse", "HEAD^"]), main, "{text}");
    assert!(!store.paths().trackers.join("project.toml").exists());
    let access =
        std::fs::read_to_string(store.paths().trackers.join("access.toml")).expect("access");
    assert!(access.contains("mount = \".newgit/trackers\""), "{access}");

    let manager = BranchManager::open(store.clone()).expect("reopen");
    assert!(!manager.legacy_layout());
    let tracker_store = store.paths().tracker_store.clone();
    let root = root_main(&store);
    let root_files = git(
        &repo,
        &[
            &format!("--git-dir={tracker_store}"),
            "ls-tree",
            "-r",
            "--name-only",
            &root,
        ],
    );
    assert!(root_files.contains(".newgit/pins/access"), "{root_files}");
    assert!(
        root_files.contains(".newgit/config.toml"),
        "root holds `.newgit`"
    );
    assert!(!root_files.contains(".newgit/trackers/"), "{root_files}");
    assert_eq!(
        git(
            &repo,
            &[
                &format!("--git-dir={tracker_store}"),
                "rev-parse",
                &format!("{root}^")
            ]
        ),
        main,
        "root's history is the view's"
    );
    // The old refs are kept.
    assert_eq!(
        git(
            &repo,
            &[
                &format!("--git-dir={tracker_store}"),
                "rev-parse",
                "refs/newgit-premigration/trackers/project/main"
            ]
        ),
        git(&repo, &["rev-parse", "HEAD~2"])
    );

    // Composed from tracker commits alone, every view is itself: `main`, the
    // signed commit under it, and the instance.
    let fresh = temp.join("fresh");
    git(&temp, &["init", "-q", "--bare", fresh.as_str()]);
    git(
        &fresh,
        &[
            "fetch",
            "-q",
            tracker_store.as_str(),
            "+refs/trackers/*:refs/trackers/*",
        ],
    );
    let everything = |_: &str| true;
    let views = compose::compose(
        &fresh,
        "root",
        &[
            ("refs/heads/main".to_owned(), root.clone()),
            ("refs/heads/feature".to_owned(), feature.clone()),
        ],
        &ComposeOptions {
            readable: &everything,
            admit: None,
        },
    )
    .expect("compose");
    assert_eq!(views[0].tip.as_deref(), Some(head.as_str()));
    assert_eq!(views[1].tip.as_deref(), Some(feature.as_str()));
    assert!(git(&fresh, &["cat-file", "commit", &main]).contains("gpgsig"));

    // Work goes on: the instance's old commits are their own root commits,
    // and its next merge into `main` is an ordinary one.
    let again = manager.migrate(false).expect("again");
    assert!(again.already, "{:?}", again.lines);
    manager.merge("main", "main").expect("merge main");
}
