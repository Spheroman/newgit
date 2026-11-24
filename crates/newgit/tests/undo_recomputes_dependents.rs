//! An undo that recomputes a resource recomputes the `recompute`
//! dependents after it, even when their own identity did not move.
//!
//! Found setting up Remix + Prisma: `deps` installs `node_modules` (and is
//! filled from the install store), `prisma-client` depends on it and runs
//! `prisma generate`, which writes `node_modules/.prisma/client`. With
//! `node_modules` deleted, `undo` refilled it from the store, then skipped
//! `prisma-client` as "identity unchanged" and printed "Restored … ok", and
//! the app crashed with `Cannot find module '.prisma/client/default'`. A
//! tree rebuilt from its inputs holds nothing a dependent generated into it.
//!
//! `start` after `stop --evict` brings the workspace back through the same
//! undo, and had the same hole.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

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
    let dir = std::env::temp_dir().join(format!(
        "newgit-undo-recomputes-dependents-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

/// The project: a lockfile, a schema, and `node_modules` ignored.
fn setup(temp: &Utf8Path) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("prisma")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("package-lock.json"), "lock 1\n").expect("write");
    std::fs::write(repo.join("prisma/schema.prisma"), "model A {}\n").expect("write");
    std::fs::write(repo.join(".gitignore"), "node_modules\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    std::fs::write(store.paths().resources.join("deps.toml"), DEPS).expect("write");
    std::fs::write(
        store.paths().resources.join("prisma-client.toml"),
        PRISMA_CLIENT,
    )
    .expect("write");
    store
}

fn newgit(repo: &Utf8Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(repo.as_str())
        .args(args)
        .output()
        .expect("newgit runs");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `npm ci`, in the store: it starts from an empty tree.
const DEPS: &str = r#"ownership = "branch"

[identity]
paths = ["package-lock.json"]
produces = ["node_modules"]

[actions.prepare]
command = "rm -rf node_modules && mkdir -p node_modules/pkg && echo installed > node_modules/pkg/index.js"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
"#;

/// `prisma generate`, as the agent first wrote it: no `produces`, so only
/// its dependency on `deps` can tell the undo its output went.
const PRISMA_CLIENT: &str = r#"ownership = "branch"
depends_on = ["deps"]

[identity]
paths = ["prisma/schema.prisma"]

[actions.prepare]
command = "mkdir -p node_modules/.prisma/client && cp prisma/schema.prisma node_modules/.prisma/client/default"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
"#;

const GENERATED: &str = "node_modules/.prisma/client/default";

#[test]
fn an_undo_that_refills_a_tree_regenerates_what_a_dependent_wrote_into_it() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let workspace = temp.join("workspaces/x");
    assert!(workspace.join(GENERATED).is_file(), "{stdout}");
    let (code, stdout, stderr) = newgit(&repo, &["checkpoint", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    // Nothing moved: both skip, as before.
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x", "--to", "ckpt_001"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("resource: prisma-client recompute(prepare) skipped: identity unchanged"),
        "{stdout}"
    );

    std::fs::remove_dir_all(workspace.join("node_modules")).expect("rm node_modules");
    let (code, stdout, stderr) = newgit(&repo, &["undo", "x", "--to", "ckpt_001"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("resource: deps recompute(prepare) filled from install store"),
        "{stdout}"
    );
    assert!(
        stdout
            .contains("resource: prisma-client recompute(prepare) after `deps` was recomputed ok"),
        "{stdout}"
    );
    assert!(
        workspace.join(GENERATED).is_file(),
        "the generated client is back:\n{stdout}"
    );
    assert!(workspace.join("node_modules/pkg/index.js").is_file());
}

#[test]
fn start_after_an_eviction_regenerates_what_a_dependent_wrote_into_a_refilled_tree() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    let (code, stdout, stderr) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let (code, stdout, stderr) = newgit(&repo, &["stop", "--evict", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let workspace = temp.join("workspaces/x");
    assert!(!workspace.join("node_modules").exists(), "{stdout}");

    let (code, stdout, stderr) = newgit(&repo, &["start", "x"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(
        workspace.join(GENERATED).is_file(),
        "the generated client is back:\n{stdout}{stderr}"
    );
}
