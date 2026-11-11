// A tracker moving between stores (#409's review, then #410): copied into its
// new store, then (once the landing that moves it puts readers there) removed
// where it was, so a move that stops part way never leaves readers pointed at
// a store without the tracker. A ref removed must still be what was copied: a
// push that moved one meanwhile is carried to the new store, never dropped.

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
fn a_move_copies_then_removes() {
    let home = tempdir("moves-order");
    let devs = Repo::new(&home, Store::ProjectDevs, "t/root".into());
    with_main(&devs, &home, "root");
    let site = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    let commit = with_main(&site, &home, "site");
    let public = Repo::new(&home, Store::Public, "t/site".into());
    let copied = site.copy_to(&Store::Public).expect("copied");
    // Copied whole, and still where it was: readers are not moved yet.
    assert_eq!(copied.to, public);
    assert_eq!(public.branch_head("main").as_deref(), Some(commit.as_str()));
    assert_eq!(site.branch_head("main").as_deref(), Some(commit.as_str()));
    site.finish_move(&copied).expect("moved");
    assert!(!site.exists() && site.branch_head("main").is_none());
    assert!(public.exists() && devs.exists());
    // Done again (a landing replayed at startup): nothing left to do.
    let again = site.copy_to(&Store::Public).expect("copied again");
    site.finish_move(&again).expect("moved again");
    assert_eq!(public.branch_head("main").as_deref(), Some(commit.as_str()));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_copy_into_a_store_for_another_audience_moves_nothing() {
    let home = tempdir("moves-wrong-store");
    let site = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    let commit = with_main(&site, &home, "site");
    // `public.git` made for someone else is in the way.
    repos::create_store(&Store::Public.dir(&home), &Store::User("eve".into()))
        .expect("made for eve");
    assert!(site.copy_to(&Store::Public).is_err());
    assert!(site.exists());
    assert_eq!(site.branch_head("main").as_deref(), Some(commit.as_str()));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_push_during_a_move_is_carried_to_the_new_store() {
    let home = tempdir("moves-race");
    let site = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    with_main(&site, &home, "site");
    let other = Repo::new(&home, Store::ProjectDevs, "t/other".into());
    let pushed = with_main(&other, &home, "pushed meanwhile");
    let copied = site.copy_to(&Store::Public).expect("copied");
    // A push that began before readers moved lands on the old store between
    // the copy and the remove, naming another tracker's commit (`other`'s),
    // and the move carries that commit into the wider store. That is
    // acceptable only because of who can make such a push: a push into the
    // old store is authorized for its audience, so the pusher already reads
    // every object there, `other`'s included, and pointing `site` at one is
    // publishing what they could read, as pushing its content to `site`
    // after the move would. Nothing is carried that its pusher could not
    // read. The precondition, asserted: the commit was in the store the push
    // went to before it, and that store is the narrower of the two.
    assert!(
        Store::Public.includes(&site.store) && !site.store.includes(&Store::Public),
        "the move widens: the push went into the narrower store"
    );
    assert!(
        repos::has_commit(&site.dir(), &pushed),
        "the pusher could read the commit where they pushed it"
    );
    site.set_refs(&[("refs/heads/main".to_owned(), pushed.clone())])
        .expect("pushed");
    site.set_refs(&[("refs/heads/topic".to_owned(), pushed.clone())])
        .expect("pushed a new branch");
    site.finish_move(&copied).expect("moved");
    let public = copied.to;
    assert_eq!(public.branch_head("main").as_deref(), Some(pushed.as_str()));
    assert_eq!(
        public.branch_head("topic").as_deref(),
        Some(pushed.as_str())
    );
    assert!(!site.exists() && site.branch_head("main").is_none());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_push_to_the_new_store_after_readers_moved_wins() {
    let home = tempdir("moves-newer");
    let site = Repo::new(&home, Store::ProjectDevs, "t/site".into());
    with_main(&site, &home, "site");
    let other = Repo::new(&home, Store::ProjectDevs, "t/other".into());
    let old_push = with_main(&other, &home, "old store");
    let copied = site.copy_to(&Store::Public).expect("copied");
    let third = Repo::new(&home, Store::Public, "t/third".into());
    let newer = with_main(&third, &home, "new store");
    copied
        .to
        .set_refs(&[("refs/heads/main".to_owned(), newer.clone())])
        .expect("pushed to the new store");
    site.set_refs(&[("refs/heads/main".to_owned(), old_push)])
        .expect("pushed to the old store");
    site.finish_move(&copied).expect("moved");
    assert_eq!(
        copied.to.branch_head("main").as_deref(),
        Some(newer.as_str())
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_seed_alone_is_labelled_before_it_is_renamed() {
    let home = tempdir("moves-rename");
    let seed = Repo::new(&home, Store::Tracker("site".into()), "t/site".into());
    with_main(&seed, &home, "site");
    let copied = seed.copy_to(&Store::Public).expect("renamed");
    assert!(copied.renamed);
    let public = Store::Public.dir(&home);
    assert_eq!(repos::audience_of(&public).as_deref(), Some("public"));
    assert!(Repo::new(&home, Store::Public, "t/site".into()).exists());
    assert!(!home.join("stores/tracker").exists());
    seed.finish_move(&copied).expect("nothing left to remove");
    let _ = std::fs::remove_dir_all(&home);
}
