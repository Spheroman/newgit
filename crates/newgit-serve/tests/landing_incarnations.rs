// A project deleted and made again under the same name is another project
// (#435's review, blocker 2). Every entry names the incarnation it was
// written for and acts on that one only; a landing in place is never put in
// place again, because SQLite records each stamp in the transaction that
// writes its rows; and `?at=` on the new project never shows the old one's
// lists.

mod support;

use newgit_serve::App;
use newgit_serve::db;
use newgit_serve::landing::{Change, Entry, Landing};
use support::{
    HOME, config, jam, landing, landing_in, make_project, project_with_root, server, tempdir,
};

fn delete(app: &App, home: &str) -> u64 {
    app.log
        .land(
            app,
            Landing {
                entry: Entry {
                    by: "jack".into(),
                    homes: [(
                        home.to_owned(),
                        Change {
                            incarnation: support::incarnation(app, home),
                            removed: true,
                            ..Change::default()
                        },
                    )]
                    .into(),
                    ..Entry::default()
                },
                ..Landing::default()
            },
        )
        .expect("deleted")
        .stamp
}

/// `jack/p` deleted and made again, with a `main` of its own. Returns the
/// old main's stamp, the delete's, and the new main's commit.
fn made_again(app: &App) -> (u64, u64, String) {
    let old_main = app.log.tip();
    let deleted = delete(app, HOME);
    assert!(db::project(&app.db.lock(), HOME).expect("read").is_none());
    let (_root, commits) = project_with_root(app, HOME, 1);
    app.log
        .land(app, landing(app, "main", None, &commits[0]))
        .expect("the new main");
    (old_main, deleted, commits[0].clone())
}

fn assert_new_project(app: &App, commit: &str) {
    let db = app.db.lock();
    assert!(
        db::project(&db, HOME).expect("read").is_some(),
        "the project is there"
    );
    assert_eq!(
        db::list_of(&db, HOME, "main")
            .expect("read")
            .map(|list| list["root"].clone())
            .as_deref(),
        Some(commit)
    );
    drop(db);
    let root = newgit_serve::repos::Repo::new(
        &app.config.data.join("homes/jack/p"),
        newgit_serve::repos::Store::ProjectDevs,
        "t/root".into(),
    );
    assert_eq!(root.branch_head("main").as_deref(), Some(commit));
}

#[test]
fn a_delete_replayed_never_touches_the_project_made_again() {
    let data = tempdir("incarnation-replay");
    let (app, _root, commits) = server(&data, 1);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("main");
    let (_, deleted, commit) = made_again(&app);
    // As if every landing from the delete on were never marked in place (the
    // old applied mark, held back): replay sees them all again.
    {
        let db = app.db.lock();
        db.execute_batch("DELETE FROM landings_placed")
            .expect("unmarked");
        db::set_applied(&db, deleted - 1).expect("held back");
    }
    drop(app);
    let app = App::open(config(&data)).expect("starts");
    assert_new_project(&app, &commit);
    assert!(app.log.stuck().is_empty(), "{:?}", app.log.stuck());
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_landing_stuck_elsewhere_never_makes_a_restart_replay_a_delete() {
    let data = tempdir("incarnation-stuck");
    let (app, _root, commits) = server(&data, 1);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("main");
    // Another project's landing is stuck, holding the applied mark back.
    let (bad_root, bad_commits) = project_with_root(&app, "jack/bad", 1);
    jam(&bad_root.dir());
    app.log
        .land(
            &app,
            landing_in(&app, "jack/bad", "doomed", None, &bad_commits[0]),
        )
        .expect_err("stuck");
    let (_, _, commit) = made_again(&app);
    drop(app);
    let app = App::open(config(&data)).expect("starts");
    assert_new_project(&app, &commit);
    assert_eq!(app.log.stuck().len(), 1, "only jack/bad's is stuck");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn state_at_a_stamp_of_the_deleted_project_is_not_the_new_ones() {
    let data = tempdir("incarnation-at");
    let (app, _root, commits) = server(&data, 1);
    let main = app
        .log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("main")
        .stamp;
    let old = support::incarnation(&app, HOME);
    assert!(
        app.log
            .list_at(HOME, &old, "main", main, false)
            .expect("read")
            .is_some()
    );
    made_again(&app);
    let new = support::incarnation(&app, HOME);
    assert_ne!(old, new);
    assert_eq!(
        app.log
            .list_at(HOME, &new, "main", main, false)
            .expect("read"),
        None,
        "the deleted project's main is not the new one's"
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_landing_written_for_the_deleted_project_is_refused() {
    let data = tempdir("incarnation-refused");
    let (app, _root, commits) = server(&data, 1);
    let stale = landing(&app, "late", None, &commits[0]);
    delete(&app, HOME);
    let refused = app.log.land(&app, stale).expect_err("deleted");
    assert_eq!(refused.status, 409, "{}", refused.message);
    make_project(&app, HOME);
    let stale = landing(&app, "late", None, &commits[0]);
    delete(&app, HOME);
    make_project(&app, HOME);
    let refused = app.log.land(&app, stale).expect_err("made again");
    assert_eq!(refused.status, 409, "{}", refused.message);
    assert!(
        refused.message.contains("made again"),
        "{}",
        refused.message
    );
    // Made twice: the second is refused.
    let tip = app.log.tip();
    let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        make_project(&app, HOME);
    }));
    assert!(again.is_err(), "a project made over another is refused");
    assert_eq!(app.log.tip(), tip);
    let _ = std::fs::remove_dir_all(&data);
}
