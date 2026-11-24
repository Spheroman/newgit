// A landing that is in place and not finished has landed (#435's second
// review, blocker 1).
//
// A store move whose rows are written but whose old copy could not be
// removed is in place: readers see it. Its merger is told it landed, with
// the cleanup pending, not 503. Voiding it is refused, since moving its
// refs back while its rows stay would split the API from Git; it is
// finished by trying again, at the next landing on its project or at the
// next start.

mod support;

use camino::Utf8Path;
use newgit_serve::App;
use newgit_serve::db;
use newgit_serve::landing::Move;
use newgit_serve::repos::{Repo, Store};
use support::{HOME, config, jam, landing, root_of, server, tempdir, unjam};

fn reference(dir: &Utf8Path, name: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "--git-dir",
            dir.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            name,
        ])
        .output()
        .expect("git runs");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `jack/p`'s `main` at c0, then a landing that moves it to c1 and moves
/// `root` from `project-devs` to `public`, with the old store unwritable so
/// removing `root` there fails. Returns the app, the old and new repos, the
/// commits, and the landing's stamp.
fn stuck_in_place(data: &Utf8Path) -> (App, Repo, Repo, Vec<String>, u64) {
    let (app, root, c) = server(data, 2);
    app.log
        .land(&app, landing(&app, "main", None, &c[0]))
        .expect("main");
    let mut moving = landing(&app, "main", Some(&c[0]), &c[1]);
    moving
        .entry
        .homes
        .get_mut(HOME)
        .expect("home")
        .stores
        .insert(
            "root".into(),
            Move {
                from: Some("project-devs".into()),
                to: "public".into(),
            },
        );
    jam(&root.dir());
    let landed = app
        .log
        .land(&app, moving)
        .expect("in place, so it landed: its merger is not told 503");
    let pending = landed.pending.expect("the cleanup is said to be pending");
    assert!(!pending.is_empty());
    let stamp = landed.stamp;
    {
        let db = app.db.lock();
        assert_eq!(db::placed(&db, stamp).expect("placed"), Some(false));
        assert_eq!(
            db::tracker_store(&db, HOME, "root")
                .expect("store")
                .as_deref(),
            Some("public")
        );
    }
    let stuck = app.log.stuck();
    assert_eq!(stuck.len(), 1, "{stuck:?}");
    assert!(stuck[0].placed, "known to be in place: {stuck:?}");
    let public = Repo::new(
        &data.join("homes").join(HOME),
        Store::Public,
        "t/root".into(),
    );
    (app, root, public, c, stamp)
}

/// Readers agree: the API's list and `public`'s ref are both c1.
fn agrees(app: &App, public: &Repo, commit: &str) {
    assert_eq!(root_of(app, HOME, "main").as_deref(), Some(commit));
    assert_eq!(
        reference(&public.dir(), &public.full("refs/heads/main")).as_deref(),
        Some(commit)
    );
}

#[test]
fn a_landing_in_place_is_not_voided_and_is_finished_when_tried_again() {
    let data = tempdir("void-in-place-retry");
    let (app, root, public, c, stamp) = stuck_in_place(&data);
    agrees(&app, &public, &c[1]);

    let refused = app.log.void(&app, stamp).expect_err("in place");
    assert_eq!(refused.status, 409, "{}", refused.message);
    assert!(
        refused.message.contains("is in place") && refused.message.contains("cannot be voided"),
        "{}",
        refused.message
    );
    assert_eq!(app.log.tip(), stamp, "nothing written");
    agrees(&app, &public, &c[1]);

    // While its old copy stays, its project refuses new landings, saying so.
    let busy = app
        .log
        .land(&app, landing(&app, "next", None, &c[1]))
        .expect_err("not finished");
    assert_eq!(busy.status, 503, "{}", busy.message);
    assert!(busy.message.contains("is in place"), "{}", busy.message);
    assert!(!busy.message.contains("--void-landing"), "{}", busy.message);

    // The cause gone, the next landing there finishes it first.
    unjam(&root.dir());
    app.log
        .land(&app, landing(&app, "next", None, &c[1]))
        .expect("lands");
    assert!(app.log.stuck().is_empty());
    assert_eq!(app.log.applied(), app.log.tip());
    assert_eq!(
        reference(&root.dir(), &root.full("refs/heads/main")),
        None,
        "the old store's copy is removed"
    );
    agrees(&app, &public, &c[1]);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_landing_in_place_is_finished_at_the_next_start() {
    let data = tempdir("void-in-place-restart");
    let (app, root, public, c, stamp) = stuck_in_place(&data);
    assert_eq!(app.log.void(&app, stamp).expect_err("in place").status, 409);
    drop(app);
    unjam(&root.dir());
    let app = App::open(config(&data)).expect("starts");
    assert!(app.log.stuck().is_empty(), "{:?}", app.log.stuck());
    assert_eq!(app.log.applied(), app.log.tip());
    assert_eq!(reference(&root.dir(), &root.full("refs/heads/main")), None);
    agrees(&app, &public, &c[1]);
    // And through the CLI's path too, it is not voided: nothing is stuck.
    drop(app);
    let refused = newgit_serve::void_landing(config(&data), stamp).expect_err("not stuck");
    assert!(refused.to_string().contains("not stuck"), "{refused}");
    let _ = std::fs::remove_dir_all(&data);
}
