//! Capture takes back only what the render wrote (#260).
//!
//! A tracker-owned rendered file is reversed before it is recorded, so this
//! instance's port never reaches the lane. Reversing every occurrence of the
//! value also rewrote text the user typed that happened to equal it, and
//! left a lane holding the `find` twice, which no instance could render
//! again. These pin the fix: the render's own spans are reversed, the
//! user's text is kept, and a capture that cannot do that cleanly is
//! refused, naming the line.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::config::WorkspaceSection;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;
use newgit_core::tracker::Storage;
use newgit_core::{NewgitError, SourceSubstrate};

const ENV: &str = "SECRET=one\nWEB=http://127.0.0.1:8000\n";

const WEB: &str = r#"ownership = "branch"
depends_on = ["env"]

[ports]
app = { start = 18000 }

[[render]]
path = ".env.local"
replace = [{ find = "127.0.0.1:8000", with = "127.0.0.1:{{ports.app}}" }]
"#;

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

/// A project whose `.env.local` belongs to the secret tracker `env`, and a
/// `web` resource rendering its port into it.
fn project(temp: &Utf8Path) -> BranchManager {
    let repo = temp.join("store");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    std::fs::write(repo.join(".env.local"), ENV).expect("write env");

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store.write_resource_file("web", WEB).expect("resource");

    let m = BranchManager::open(store).expect("manager");
    m.create_tracker("env", "user", Storage::Local, false)
        .expect("create tracker");
    m.track_paths("env", &[Utf8PathBuf::from(".env.local")])
        .expect("track");
    m.seed_tracker_from_store("env").expect("seed");
    BranchManager::open(MetadataStore::at(m.store().paths().project_root.clone())).expect("manager")
}

fn spawn(m: &BranchManager, name: &str) -> (Utf8PathBuf, u16) {
    let spawned = m.spawn(name, None).expect("spawn");
    let web = spawned
        .resources
        .iter()
        .find(|outcome| outcome.name == "web")
        .expect("web");
    assert!(web.render_error.is_none(), "{:?}", web.render_error);
    (
        spawned.branch.workspace_path.clone(),
        spawned.branch.resources["web"].resolved_ports["app"],
    )
}

fn lane(m: &BranchManager, rev: &str) -> String {
    let paths = m.store().paths();
    newgit_core::tracker_repo::TrackerRepo::new(
        &paths.tracker_store,
        &paths.tracker_checkouts,
        "env",
    )
    .read_file(rev, Utf8Path::new(".env.local"))
    .expect("read lane")
    .expect("lane content")
}

/// The repro from the issue: a line the user typed holds the same text as
/// the rendered value. It stays as typed, and the lane still renders.
#[test]
fn a_users_text_equal_to_the_rendered_value_is_kept() {
    let (_guard, temp) = tempdir();
    let m = project(&temp);
    let (workspace, port) = spawn(&m, "rev");

    let env = workspace.join(".env.local");
    let mut content = std::fs::read_to_string(&env).expect("env");
    content.push_str(&format!("MIRROR=127.0.0.1:{port}\nTIMEOUT_MS={port}\n"));
    std::fs::write(&env, &content).expect("edit");

    let report = m.capture_tracker("rev", "env").expect("capture");
    assert_eq!(
        lane(&m, &report.rev),
        format!("{ENV}MIRROR=127.0.0.1:{port}\nTIMEOUT_MS={port}\n")
    );
    assert_eq!(std::fs::read_to_string(&env).expect("env"), content);

    // The lane still renders for the next instance.
    m.merge_tracker("rev", "env").expect("merge");
    let (next, next_port) = spawn(&m, "rev2");
    assert_eq!(
        std::fs::read_to_string(next.join(".env.local")).expect("env"),
        format!(
            "SECRET=one\nWEB=http://127.0.0.1:{next_port}\nMIRROR=127.0.0.1:{port}\n\
             TIMEOUT_MS={port}\n"
        )
    );
}

/// An edit on the rendered line itself still reverses, because the value
/// occurs exactly as often as the render wrote it: there is only one thing
/// it can be.
#[test]
fn an_edit_on_the_rendered_line_still_reverses_a_unique_value() {
    let (_guard, temp) = tempdir();
    let m = project(&temp);
    let (workspace, port) = spawn(&m, "rev");

    std::fs::write(
        workspace.join(".env.local"),
        format!("SECRET=one\nWEB=https://127.0.0.1:{port}\n"),
    )
    .expect("edit");
    let report = m.capture_tracker("rev", "env").expect("capture");
    assert_eq!(
        lane(&m, &report.rev),
        "SECRET=one\nWEB=https://127.0.0.1:8000\n"
    );
}

/// The rendered line duplicated: which copy the render wrote cannot be
/// told, so nothing is guessed. Capture refuses, naming the lines, and the
/// workspace file is left as it was.
#[test]
fn a_value_newgit_cannot_place_refuses_the_capture() {
    let (_guard, temp) = tempdir();
    let m = project(&temp);
    let (workspace, port) = spawn(&m, "rev");

    let env = workspace.join(".env.local");
    let content = format!("SECRET=one\nWEB=http://127.0.0.1:{port}\nWEB=http://127.0.0.1:{port}\n");
    std::fs::write(&env, &content).expect("edit");

    let error = m.capture_tracker("rev", "env").expect_err("refused");
    assert!(
        matches!(error, NewgitError::RenderCaptureRefused { .. }),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("line(s) 2, 3"), "{message}");
    assert!(message.contains(&format!("127.0.0.1:{port}")), "{message}");
    assert_eq!(std::fs::read_to_string(&env).expect("env"), content);

    // A checkpoint is not refused: it is the safety net, and refusing it
    // would lose the work it exists to keep.
    m.checkpoint("rev", Some("still works"))
        .expect("checkpoint");
}

/// The committed default typed by hand somewhere else: reversing is exact,
/// but the lane would then hold the `find` twice and render for nobody.
#[test]
fn content_that_would_no_longer_render_refuses_the_capture() {
    let (_guard, temp) = tempdir();
    let m = project(&temp);
    let (workspace, _) = spawn(&m, "rev");

    let env = workspace.join(".env.local");
    let mut content = std::fs::read_to_string(&env).expect("env");
    content.push_str("FALLBACK=127.0.0.1:8000\n");
    std::fs::write(&env, &content).expect("edit");

    let error = m.capture_tracker("rev", "env").expect_err("refused");
    let message = error.to_string();
    assert!(message.contains("would no longer render"), "{message}");
    assert!(message.contains("`127.0.0.1:8000`"), "{message}");
    assert!(message.contains("line(s) 2, 3"), "{message}");
    assert_eq!(std::fs::read_to_string(&env).expect("env"), content);
}

#[test]
fn a_removed_rendered_line_says_it_would_no_longer_render() {
    let (_guard, temp) = tempdir();
    let m = project(&temp);
    let (workspace, _) = spawn(&m, "rev");

    let env = workspace.join(".env.local");
    std::fs::write(&env, "SECRET=one\n").expect("edit");

    let error = m.capture_tracker("rev", "env").expect_err("refused");
    let message = error.to_string();
    assert!(message.contains("would no longer render"), "{message}");
    assert!(!message.contains("still holds"), "{message}");
}
