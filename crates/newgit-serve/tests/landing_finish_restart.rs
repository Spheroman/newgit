// A store move put in place before a restart and not finished (#435's
// review, item 7). The copy only runs before readers move, which the
// transaction that moves them records; after it, the replay only finishes
// the move, carrying what is left where the tracker was forward and never
// moving a ref in the new store back.

use camino::{Utf8Path, Utf8PathBuf};
use newgit_serve::repos::{Repo, Store};

fn tempdir(name: &str) -> Utf8PathBuf {
    let dir = std::env::temp_dir().join(format!("newgit-serve-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonical")).expect("utf8")
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Three commits on one line, oldest first, pushed nowhere yet.
fn line(work: &Utf8Path) -> Vec<String> {
    std::fs::create_dir_all(work).expect("mkdir");
    git(work, &["init", "-q", "-b", "main"]);
    (0..3)
        .map(|n| {
            std::fs::write(work.join("file"), n.to_string()).expect("write");
            git(work, &["add", "."]);
            git(work, &["commit", "-q", "-m", &n.to_string()]);
            git(work, &["rev-parse", "HEAD"])
        })
        .collect()
}

fn push(work: &Utf8Path, repo: &Repo, commit: &str) {
    git(
        work,
        &[
            "push",
            "-q",
            "-f",
            repo.dir().as_str(),
            &format!("{commit}:{}refs/heads/main", repo.prefix()),
        ],
    );
}

#[test]
fn a_push_to_the_new_store_after_readers_moved_is_never_moved_back() {
    let home = tempdir("finish-newer");
    let work = home.join("work");
    let commits = line(&work);
    let was = Repo::new(&home, Store::Public, "t/site".into());
    let now = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    was.create().expect("old");
    now.create().expect("new");
    // Copied at the first commit; pushed on to the third in the new store
    // after readers moved there.
    push(&work, &was, &commits[0]);
    push(&work, &now, &commits[2]);
    was.finish_move_after_restart(&now).expect("finished");
    assert_eq!(
        now.branch_head("main").as_deref(),
        Some(commits[2].as_str())
    );
    assert_eq!(was.branch_head("main"), None, "removed where it was");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_push_left_where_the_tracker_was_is_carried_forward() {
    let home = tempdir("finish-older");
    let work = home.join("work");
    let commits = line(&work);
    let was = Repo::new(&home, Store::Public, "t/site".into());
    let now = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    was.create().expect("old");
    now.create().expect("new");
    // A push in flight landed where it was, after the copy.
    push(&work, &now, &commits[1]);
    push(&work, &was, &commits[2]);
    was.finish_move_after_restart(&now).expect("finished");
    assert_eq!(
        now.branch_head("main").as_deref(),
        Some(commits[2].as_str())
    );
    assert_eq!(was.branch_head("main"), None);
    let _ = std::fs::remove_dir_all(&home);
}
