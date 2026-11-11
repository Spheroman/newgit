// A tracker moving between stores (#409's review): copied, then placed (its
// row follows), then removed where it was, so a move that stops part way never
// leaves the row naming a store without the tracker. A ref removed must still
// be what was copied: a push that moved one meanwhile fails the move, and the
// row goes back, rather than the push being dropped.

use std::cell::RefCell;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_serve::repos::{self, Repo, Store};

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

/// `repo` made, with `main` at a new commit; returns the commit.
fn with_main(repo: &Repo, scratch: &Utf8Path, text: &str) -> String {
    repo.create().expect("made");
    let work = scratch.join(format!("work-{}", repo.namespace.replace('/', "_")));
    let _ = std::fs::remove_dir_all(&work);
    git(scratch, &["init", "-q", "-b", "main", work.as_str()]);
    std::fs::write(work.join("file"), text).expect("write");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", text]);
    let commit = git(&work, &["rev-parse", "HEAD"]);
    git(
        &work,
        &[
            "push",
            "-q",
            repo.dir().as_str(),
            &format!("HEAD:{}refs/heads/main", repo.prefix()),
        ],
    );
    commit
}

#[test]
fn a_move_copies_then_places_then_removes() {
    let home = tempdir("moves-order");
    let devs = Repo::new(&home, Store::ProjectDevs, "t/root".into());
    with_main(&devs, &home, "root");
    let site = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    let commit = with_main(&site, &home, "site");
    let public = Repo::new(&home, Store::Public, "t/site".into());
    let placed = RefCell::new(Vec::new());
    let moved = site
        .move_to(&Store::Public, |store| {
            // When the row follows, the copy is whole and the old one is
            // still there.
            assert_eq!(public.branch_head("main").as_deref(), Some(commit.as_str()));
            assert_eq!(site.branch_head("main").as_deref(), Some(commit.as_str()));
            placed.borrow_mut().push(store.clone());
            Ok(())
        })
        .expect("moved");
    assert_eq!(moved, public);
    assert_eq!(*placed.borrow(), [Store::Public]);
    assert!(!site.exists() && site.branch_head("main").is_none());
    assert!(public.exists() && devs.exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_move_whose_row_cannot_follow_leaves_the_tracker_where_it_was() {
    let home = tempdir("moves-place-fails");
    let site = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    let commit = with_main(&site, &home, "site");
    let refused = site.move_to(&Store::Public, |_| {
        Err(newgit_serve::error::HttpError::internal("the db is gone"))
    });
    assert!(refused.is_err());
    assert!(site.exists());
    assert_eq!(site.branch_head("main").as_deref(), Some(commit.as_str()));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_push_during_a_move_fails_the_move_and_is_kept() {
    let home = tempdir("moves-race");
    let site = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    with_main(&site, &home, "site");
    let other = Repo::new(&home, Store::ProjectDevs, "t/other".into());
    let pushed = with_main(&other, &home, "pushed meanwhile");
    let placed = RefCell::new(Vec::new());
    let refused = site.move_to(&Store::Public, |store| {
        if placed.borrow().is_empty() {
            // A push lands on the old store between the copy and the remove.
            site.set_refs(&[("refs/heads/main".to_owned(), pushed.clone())])
                .expect("pushed");
        }
        placed.borrow_mut().push(store.clone());
        Ok(())
    });
    assert!(refused.is_err(), "the move dropped a push");
    // The row went to the new store, then back.
    assert_eq!(*placed.borrow(), [Store::Public, Store::ProjectDevs]);
    assert_eq!(site.branch_head("main").as_deref(), Some(pushed.as_str()));
    assert!(site.exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_seed_alone_is_labelled_before_it_is_renamed() {
    let home = tempdir("moves-rename");
    let seed = Repo::new(&home, Store::Tracker("site".into()), "t/site".into());
    with_main(&seed, &home, "site");
    // The row cannot follow: the store is where it was going, labelled for
    // that audience, for `settle` to place the row next time.
    let refused = seed.move_to(&Store::Public, |_| {
        Err(newgit_serve::error::HttpError::internal("the db is gone"))
    });
    assert!(refused.is_err());
    let public = Store::Public.dir(&home);
    assert_eq!(repos::audience_of(&public).as_deref(), Some("public"));
    assert!(Repo::new(&home, Store::Public, "t/site".into()).exists());
    let _ = std::fs::remove_dir_all(&home);
}
