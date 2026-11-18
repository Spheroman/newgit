//! What an agent setting a project up had to find by searching: where logs
//! are, that the workspaces directory can move, that definitions are read
//! from the project root, and how to split generated code out of an install.
//! Each is said where the agent is already looking.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::installs::InstallReport;
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

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn repo() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-briefing-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let temp = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    (TempDir(temp), repo)
}

fn newgit(dir: &Utf8Path, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
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

#[test]
fn init_says_the_workspaces_directory_can_move() {
    let (_temp, repo) = repo();
    let (code, output) = newgit(&repo, &["init"]);
    assert_eq!(code, Some(0), "{output}");
    let at = output
        .lines()
        .position(|line| line.trim_start().starts_with("workspaces:"))
        .expect("a workspaces line");
    assert!(
        output
            .lines()
            .nth(at + 1)
            .is_some_and(|line| line.contains("`[workspace] root` in .newgit/config.toml")),
        "{output}"
    );
}

#[test]
fn status_names_the_log_of_a_failed_prepare() {
    let (temp, repo) = repo();
    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.0.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    std::fs::write(
        store.paths().resources.join("deps.toml"),
        "ownership = \"workspace\"\n\n[actions.prepare]\ncommand = \"echo broke; exit 3\"\n",
    )
    .expect("write resource");

    let (code, output) = newgit(&repo, &["spawn", "x"]);
    assert_eq!(code, Some(1), "{output}");
    let (_, output) = newgit(&repo, &["status", "x"]);
    let line = output
        .lines()
        .find(|line| line.contains("`deps` failed"))
        .unwrap_or_else(|| panic!("{output}"));
    let log = line
        .split("its log is ")
        .nth(1)
        .unwrap_or_else(|| panic!("names no log: {line}"))
        .trim();
    let logs = store.paths().logs.join("x");
    assert!(
        log.starts_with(&format!("{logs}/deps.prepare-")) && log.ends_with(".log"),
        "{line}"
    );
    assert!(
        std::fs::read_to_string(log).expect("log").contains("broke"),
        "{log}"
    );
    let _ = newgit(&repo, &["remove", "x", "--force"]);
}

#[test]
fn the_reference_says_what_an_agent_had_to_search_for() {
    let (_temp, repo) = repo();
    let (_, agents) = newgit(&repo, &["reference", "agents"]);
    for needle in [
        "**Edit `.newgit/` in the project root, never in a workspace.**",
        "**Give every port explicitly.**",
        "**Build before you serve.**",
        "**`[ready]` says what \"up\" means**",
        "**An edit does not reach a live instance.**",
        "`.newgit/logs/<instance>/`",
    ] {
        assert!(agents.contains(needle), "agents lacks {needle}");
    }
    let (_, actions) = newgit(&repo, &["reference", "actions"]);
    assert!(actions.contains("**A `long_running` action's stdin stays open.**"));
    assert!(actions.contains("`<resource>.<action>-<UTC time>.log`"));
    let (code, generated) = newgit(&repo, &["reference", "generated-code"]);
    assert_eq!(code, Some(0), "{generated}");
    assert!(generated.contains("PRISMA_SKIP_POSTINSTALL_GENERATE=1 npm ci"));
    assert!(generated.contains("produces = [\"node_modules/.prisma\"]"));
}

/// The store's refusal offers both ways out and says when each fits.
#[test]
fn a_declined_tree_names_both_fixes() {
    let summary = InstallReport::Declined {
        key: "3ec46aa385bb".to_owned(),
        path: Utf8PathBuf::from("node_modules/.prisma/client/index.js"),
    }
    .summary();
    assert!(summary.contains("a `[[render]]` on it"), "{summary}");
    assert!(summary.contains("generated code"), "{summary}");
    assert!(summary.contains("`depends_on` this one"), "{summary}");
    assert!(
        summary.contains("newgit reference generated-code"),
        "{summary}"
    );
}
