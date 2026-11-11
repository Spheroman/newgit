// A stored rooted history never moves back (#412's review, item 2), and its
// refs move by a real compare-and-swap against the state its writer read:
//
// - refreshed to newer lists and then to older ones, it stays at the newer;
//   and the same history read again later says so, so a writer that read
//   the lists in between (A, then B, then A again) cannot put B back;
// - a writer that read the state before another wrote it loses the swap,
//   reads it again, and stops at the newer one;
// - an extension racing its tracker's move out of the store (the tracker's
//   own `HEAD` there removed, then its stored history) writes nothing back
//   there, whether the history was there already or not;
// - the namespace's `HEAD` moves in the same transaction as the state: a
//   stop right after a write leaves it set, and a write that fails moves
//   neither.

mod support;

use std::cell::Cell;

use camino::Utf8Path;
use newgit_serve::App;
use newgit_serve::api::ProjectContext;
use newgit_serve::names::{ProjectId, parse_project_key};
use newgit_serve::repos::Store;
use newgit_serve::rooted::{self, Snapshot, Want};
use support::{HOME, jam, landing, server, tempdir, unjam};

fn id() -> ProjectId {
    parse_project_key(HOME).expect("a key")
}

/// Root's stored rooted history as the lists have it now, checked where it is.
fn want_now(app: &App) -> Want {
    let mut want = {
        let db = app.db.lock();
        let project = ProjectContext::open(app, &db, &id()).expect("project");
        Snapshot::read(&project, &db)
            .expect("snapshot")
            .want("root")
            .expect("root has one")
    };
    assert!(rooted::located(&mut want, &id()).expect("located"));
    want
}

fn git(dir: &Utf8Path, args: &[&str]) -> Option<String> {
    let output = newgit_core::git::command()
        .arg("--git-dir")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn served_main(dir: &Utf8Path) -> Option<String> {
    git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/namespaces/r/refs/namespaces/root/refs/heads/main",
        ],
    )
}

fn state_served(dir: &Utf8Path) -> Option<String> {
    git(dir, &["cat-file", "blob", "refs/newgit/rooted/root:served"])
}

fn source_of(dir: &Utf8Path) -> Option<String> {
    let served = state_served(dir)?;
    served
        .lines()
        .find_map(|line| line.strip_prefix("head main "))
        .and_then(|rest| rest.split(' ').next())
        .map(str::to_owned)
}

#[test]
fn an_older_refresh_never_moves_a_newer_view_back() {
    let data = tempdir("rooted-monotonic-old");
    let (app, root, commits) = server(&data, 3);
    let dir = root.dir();
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("lands");
    let old = want_now(&app);
    app.log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
        .expect("lands");
    let new = want_now(&app);
    assert!(old.at.behind(&new.at), "{:?} before {:?}", old.at, new.at);

    let done = rooted::refresh(&new).expect("refresh");
    assert_eq!(done.composed, 2);
    let tip = served_main(&dir).expect("served");
    let done = rooted::refresh(&old).expect("refresh");
    assert!(done.newer && done.composed == 0, "{done:?}");
    assert_eq!(served_main(&dir), Some(tip.clone()), "never moved back");
    assert_eq!(source_of(&dir).as_deref(), Some(commits[1].as_str()));

    // A, then B, then A read again later: B is never put back.
    app.log
        .land(&app, landing(&app, "main", Some(&commits[1]), &commits[0]))
        .expect("lands");
    let again = want_now(&app);
    assert!(new.at.behind(&again.at));
    assert!(!rooted::refresh(&again).expect("refresh").newer);
    let first = served_main(&dir).expect("served");
    assert_ne!(first, tip);
    let mut a_later = again.clone();
    a_later.at.revision += 10;
    assert!(rooted::refresh(&a_later).expect("refresh").fresh);
    let done = rooted::refresh(&new).expect("refresh");
    assert!(done.newer, "{done:?}");
    assert_eq!(served_main(&dir), Some(first), "B is not put back");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_writer_that_read_the_state_before_another_wrote_loses_the_swap() {
    let data = tempdir("rooted-monotonic-swap");
    let (app, root, commits) = server(&data, 3);
    let dir = root.dir();
    app.log
        .land(&app, landing(&app, "main", None, &commits[1]))
        .expect("lands");
    let old = want_now(&app);
    app.log
        .land(&app, landing(&app, "main", Some(&commits[1]), &commits[2]))
        .expect("lands");
    let new = want_now(&app);

    // The old writer records its first chunk; then, between chunks, the
    // newer one writes. The old one's next swap is against the state it
    // wrote, which is no longer there.
    let between_ran = Cell::new(false);
    let between = || {
        if !between_ran.replace(true) {
            let done = rooted::refresh(&new).expect("the newer writes");
            assert!(!done.newer);
        }
    };
    let done = rooted::refresh_with(&old, 1, &|| {}, &between).expect("refresh");
    assert!(between_ran.get());
    assert!(
        done.newer,
        "the older writer stops at the newer state: {done:?}"
    );
    assert_eq!(source_of(&dir).as_deref(), Some(commits[2].as_str()));
    assert!(rooted::refresh(&new).expect("refresh").fresh);
    let _ = std::fs::remove_dir_all(&data);
}

/// What a move out of the store does to root there: its own refs and
/// `HEAD` removed, then its stored history (`Repo::remove_rooted`).
fn move_out(dir: &Utf8Path) {
    let own = "refs/namespaces/t/refs/namespaces/root/";
    let listed = git(dir, &["for-each-ref", "--format=%(refname)", own]).unwrap_or_default();
    let rooted = git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/namespaces/r/refs/namespaces/root/",
            "refs/newgit/rooted/root",
        ],
    )
    .unwrap_or_default();
    for reference in listed.lines() {
        git(dir, &["update-ref", "--no-deref", "-d", reference]);
    }
    git(dir, &["symbolic-ref", "--delete", &format!("{own}HEAD")]);
    for reference in rooted.lines() {
        git(dir, &["update-ref", "--no-deref", "-d", reference]);
    }
}

fn rooted_refs(dir: &Utf8Path) -> String {
    git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/namespaces/r/",
            "refs/newgit/rooted/",
        ],
    )
    .unwrap_or_default()
}

#[test]
fn an_extension_racing_a_store_move_writes_nothing_back() {
    let data = tempdir("rooted-monotonic-move");
    let (app, root, commits) = server(&data, 3);
    let dir = root.dir();
    app.log
        .land(&app, landing(&app, "main", None, &commits[2]))
        .expect("lands");

    // Nothing stored yet: the move lands after the extension found the
    // tracker here and before its first write.
    let want = want_now(&app);
    move_out(&dir);
    let done = rooted::refresh(&want).expect("refresh");
    assert!(done.gone, "{done:?}");
    assert_eq!(rooted_refs(&dir), "", "nothing written back");

    // Stored already, and the move lands between two chunks.
    let data = tempdir("rooted-monotonic-move-chunks");
    let (app, root, commits) = server(&data, 3);
    let dir = root.dir();
    app.log
        .land(&app, landing(&app, "main", None, &commits[2]))
        .expect("lands");
    let want = want_now(&app);
    let moved = Cell::new(false);
    let between = || {
        if !moved.replace(true) {
            assert!(!rooted_refs(&dir).is_empty(), "the first chunk was written");
            move_out(&dir);
        }
    };
    let done = rooted::refresh_with(&want, 1, &|| {}, &between).expect("refresh");
    assert!(moved.get());
    assert!(done.gone, "{done:?}");
    assert_eq!(rooted_refs(&dir), "", "nothing written back");
    let _ = std::fs::remove_dir_all(&data);
}

fn head_of(dir: &Utf8Path) -> Option<String> {
    git(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "refs/namespaces/r/refs/namespaces/root/HEAD",
        ],
    )
}

#[test]
fn head_moves_with_the_state_in_one_transaction() {
    let data = tempdir("rooted-monotonic-head");
    let (app, root, commits) = server(&data, 3);
    let dir = root.dir();
    app.log
        .land(&app, landing(&app, "main", None, &commits[2]))
        .expect("lands");
    let want = want_now(&app);
    let main = "refs/namespaces/r/refs/namespaces/root/refs/heads/main";

    // A write that fails moves neither.
    jam(&dir);
    assert!(rooted::refresh(&want).is_err());
    unjam(&dir);
    assert_eq!(state_served(&dir), None);
    assert_eq!(head_of(&dir), None);

    // Stopped right after the first write (a crash there): `HEAD` is set.
    let stopped = std::panic::catch_unwind(|| {
        rooted::refresh_with(&want, 1, &|| panic!("stopped here"), &|| {})
    });
    assert!(stopped.is_err());
    assert!(state_served(&dir).is_some(), "the first chunk is recorded");
    assert_eq!(head_of(&dir).as_deref(), Some(main));

    // And resumed, it is served whole.
    let done = rooted::refresh(&want).expect("refresh");
    assert!(done.composed > 0);
    assert_eq!(head_of(&dir).as_deref(), Some(main));
    assert!(served_main(&dir).is_some());
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn the_store_is_the_trackers_own() {
    // What every test above leans on: root's store, as the server made it.
    let data = tempdir("rooted-monotonic-store");
    let (app, root, commits) = server(&data, 1);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("lands");
    let want = want_now(&app);
    assert_eq!(want.store, Store::ProjectDevs);
    assert_eq!(want.dir(), root.dir());
    assert!(want.own_head.is_some());
    let _ = std::fs::remove_dir_all(&data);
}
