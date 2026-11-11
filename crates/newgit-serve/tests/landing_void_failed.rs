// A void that is written down and cannot be put in place never lets the
// landing it voids land (#435's second review, blocker 2, without a crash).
//
// The voided landing made a branch: its ref moved, and its rows could not
// be written (SQLite locked by another writer). The void, written down while
// SQLite is still locked, moves the ref back and cannot write its rows
// either. Once SQLite is free, the next landing on the project tries the
// stuck landings again: the void is put in place, and the landing it voids
// is not, so the API and Git agree that the branch does not exist. A start
// of the server after that agrees too.

mod support;

use std::time::Duration;

use camino::Utf8Path;
use newgit_serve::App;
use support::{config, landing_in, project_with_root, root_of, server, tempdir};

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

/// Another writer holding SQLite's write lock until dropped.
fn lock_sqlite(data: &Utf8Path) -> rusqlite::Connection {
    let other =
        rusqlite::Connection::open(data.join("server.sqlite").as_std_path()).expect("opens");
    other.execute_batch("BEGIN IMMEDIATE").expect("locks");
    other
}

#[test]
fn a_void_that_fails_never_lets_the_landing_it_voids_land() {
    let data = tempdir("void-failed");
    let (app, _root, _c) = server(&data, 1);
    let (bad_root, bad) = project_with_root(&app, "jack/bad", 1);
    app.log
        .land(&app, landing_in(&app, "jack/bad", "main", None, &bad[0]))
        .expect("main");
    let doomed = bad_root.full("refs/heads/doomed");

    let other = lock_sqlite(&data);
    let refused = app
        .log
        .land(&app, landing_in(&app, "jack/bad", "doomed", None, &bad[0]))
        .expect_err("its rows cannot be written");
    assert_eq!(refused.status, 503, "{}", refused.message);
    let stamp = app.log.tip();
    assert_eq!(
        reference(&bad_root.dir(), &doomed).as_deref(),
        Some(bad[0].as_str()),
        "its ref moved before its rows failed"
    );

    let failed = app
        .log
        .void(&app, stamp)
        .expect_err("the void's rows fail too");
    assert_eq!(failed.status, 500, "{}", failed.message);
    assert!(
        failed.message.contains("never put in place"),
        "{}",
        failed.message
    );
    let void = app.log.tip();
    assert_eq!(void, stamp + 1);
    assert_eq!(reference(&bad_root.dir(), &doomed), None, "moved back");
    // Voiding it again, or voiding the void, is refused: the void stands.
    assert_eq!(app.log.void(&app, stamp).expect_err("voided").status, 409);
    assert_eq!(app.log.void(&app, void).expect_err("a void").status, 409);

    drop(other);
    std::thread::sleep(Duration::from_millis(10));
    app.log
        .land(&app, landing_in(&app, "jack/bad", "after", None, &bad[0]))
        .expect("the void is put in place first, and the project lands");
    assert!(app.log.stuck().is_empty(), "{:?}", app.log.stuck());
    assert_eq!(app.log.applied(), app.log.tip());
    assert_eq!(root_of(&app, "jack/bad", "doomed"), None, "the API agrees");
    assert_eq!(reference(&bad_root.dir(), &doomed), None, "and Git");
    assert_eq!(
        root_of(&app, "jack/bad", "main").as_deref(),
        Some(bad[0].as_str())
    );

    drop(app);
    let app = App::open(config(&data)).expect("starts");
    assert!(app.log.stuck().is_empty());
    assert_eq!(root_of(&app, "jack/bad", "doomed"), None);
    assert_eq!(reference(&bad_root.dir(), &doomed), None);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_void_that_fails_is_put_in_place_at_the_next_start_without_the_landing_it_voids() {
    let data = tempdir("void-failed-restart");
    let (app, _root, _c) = server(&data, 1);
    let (bad_root, bad) = project_with_root(&app, "jack/bad", 1);
    let doomed = bad_root.full("refs/heads/doomed");
    let other = lock_sqlite(&data);
    app.log
        .land(&app, landing_in(&app, "jack/bad", "doomed", None, &bad[0]))
        .expect_err("stuck");
    let stamp = app.log.tip();
    app.log
        .void(&app, stamp)
        .expect_err("the void's rows fail too");
    drop(other);
    drop(app);
    let app = App::open(config(&data)).expect("starts");
    assert!(app.log.stuck().is_empty(), "{:?}", app.log.stuck());
    assert_eq!(app.log.applied(), stamp + 1);
    assert_eq!(root_of(&app, "jack/bad", "doomed"), None);
    assert_eq!(reference(&bad_root.dir(), &doomed), None);
    let _ = std::fs::remove_dir_all(&data);
}
