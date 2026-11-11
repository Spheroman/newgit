// A landing that cannot be put in place (#435's review, blocker 1).
//
// - Nothing unappliable is written down: a ref moved to a commit its store
//   lacks is refused by the check, and the landings batched with it land.
// - When a batch cannot be put in place together, its landings are put in
//   place one by one, so only the one at fault is stuck.
// - A stuck landing's real cause is in the 503 its project answers, and in
//   the server's log; trying again once the cause is gone puts it in place.
// - Across a restart it stays stuck with its cause, and an operator voids it
//   with a compensating entry: its project lands again, and `?at=` never
//   shows what it would have made.

mod support;

use std::sync::{Arc, Barrier};

use newgit_serve::App;
use newgit_serve::db;
use newgit_serve::landing::Asked;
use support::{
    BOGUS, HOME, config, jam, landing, landing_in, project_with_root, server, tempdir, unjam,
};

#[test]
fn a_commit_the_store_lacks_is_refused_and_its_batch_lands() {
    let data = tempdir("stuck-bogus");
    let (app, _root, commits) = server(&data, 1);
    let app = Arc::new(app);
    const N: usize = 32;
    const BAD: usize = N / 2;
    let start = Arc::new(Barrier::new(N));
    let threads: Vec<_> = (0..N)
        .map(|n| {
            let (app, start, commit) = (app.clone(), start.clone(), commits[0].clone());
            std::thread::spawn(move || {
                let mut asked = landing(&app, &format!("b{n}"), None, &commit);
                if n == BAD {
                    asked.asked.push(Asked {
                        home: HOME.to_owned(),
                        tracker: "root".to_owned(),
                        reference: "refs/heads/x".to_owned(),
                        old: None,
                        new: BOGUS.to_owned(),
                    });
                }
                start.wait();
                app.log
                    .land(&app, asked)
                    .map(|landed| landed.stamp)
                    .map_err(|error| (error.status, error.message))
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("joined"))
        .collect();
    let (status, message) = results[BAD].clone().expect_err("a commit nobody sent");
    assert_eq!(status, 400, "{message}");
    assert!(message.contains(BOGUS), "{message}");
    for (n, result) in results.iter().enumerate() {
        if n != BAD {
            assert!(result.is_ok(), "landing {n} beside the bad one: {result:?}");
        }
    }
    assert!(app.log.stuck().is_empty(), "nothing is stuck");
    assert_eq!(app.log.applied(), app.log.tip(), "everything is in place");
    // Its project lands as before, now and after a restart.
    app.log
        .land(&app, landing(&app, "after", None, &commits[0]))
        .expect("the project lands");
    drop(results);
    let app = Arc::try_unwrap(app).ok().expect("one");
    drop(app);
    let app = App::open(config(&data)).expect("starts");
    app.log
        .land(&app, landing(&app, "after-restart", None, &commits[0]))
        .expect("the project lands after a restart");
    // A list naming a commit nobody sent is refused, and nothing written.
    let tip = app.log.tip();
    let refused = app
        .log
        .land(&app, landing(&app, "nowhere", None, BOGUS))
        .expect_err("refused");
    assert_eq!(refused.status, 400, "{}", refused.message);
    assert_eq!(app.log.tip(), tip);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_batch_that_cannot_be_put_in_place_together_is_put_in_place_one_by_one() {
    let data = tempdir("stuck-batch");
    let (app, _root, commits) = server(&data, 1);
    let (bad_root, bad_commits) = project_with_root(&app, "jack/bad", 1);
    jam(&bad_root.dir());
    let app = Arc::new(app);
    const N: usize = 32;
    const BAD: usize = 7;
    let start = Arc::new(Barrier::new(N));
    let threads: Vec<_> = (0..N)
        .map(|n| {
            let app = app.clone();
            let start = start.clone();
            let (home, commit) = if n == BAD {
                ("jack/bad", bad_commits[0].clone())
            } else {
                (HOME, commits[0].clone())
            };
            std::thread::spawn(move || {
                let asked = landing_in(&app, home, &format!("b{n}"), None, &commit);
                start.wait();
                app.log
                    .land(&app, asked)
                    .map(|landed| landed.stamp)
                    .map_err(|error| (error.status, error.message))
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("joined"))
        .collect();
    for (n, result) in results.iter().enumerate() {
        if n != BAD {
            assert!(result.is_ok(), "landing {n} in jack/p: {result:?}");
        }
    }
    let (status, message) = results[BAD].clone().expect_err("its store refuses writes");
    assert_eq!(status, 503, "{message}");
    assert!(
        message.contains("could not be put in place") && message.contains("cannot lock references"),
        "the real cause is said: {message}"
    );
    assert!(message.contains("--void-landing"), "{message}");
    let stuck = app.log.stuck();
    assert_eq!(stuck.len(), 1, "only the one at fault is stuck: {stuck:?}");
    let stamp = stuck[0].stamp;

    // Its project answers 503 with the cause; the other lands.
    let refused = app
        .log
        .land(
            &app,
            landing_in(&app, "jack/bad", "next", None, &bad_commits[0]),
        )
        .expect_err("stuck");
    assert_eq!(refused.status, 503);
    assert!(
        refused.message.contains(&format!("landing {stamp}"))
            && refused.message.contains("cannot lock references"),
        "{}",
        refused.message
    );
    app.log
        .land(&app, landing(&app, "elsewhere", None, &commits[0]))
        .expect("another project lands");
    // Nothing it would make is shown: not its list, not its stamp.
    assert_eq!(support::root_of(&app, "jack/bad", &format!("b{BAD}")), None);
    let incarnation = support::incarnation(&app, "jack/bad");
    let ahead = app
        .log
        .list_at("jack/bad", &incarnation, &format!("b{BAD}"), stamp, false)
        .expect_err("not in place");
    assert_eq!(ahead.status, 409, "{}", ahead.message);

    // Once the cause is gone, the next landing there puts it in place first.
    unjam(&bad_root.dir());
    app.log
        .land(
            &app,
            landing_in(&app, "jack/bad", "next", None, &bad_commits[0]),
        )
        .expect("lands after the stuck one is put in place");
    assert!(app.log.stuck().is_empty());
    assert_eq!(
        support::root_of(&app, "jack/bad", &format!("b{BAD}")).as_deref(),
        Some(bad_commits[0].as_str())
    );
    assert_eq!(app.log.applied(), app.log.tip());
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_landing_stuck_across_a_restart_says_why_and_is_voided_by_the_operator() {
    let data = tempdir("stuck-restart");
    let (app, _root, commits) = server(&data, 1);
    let (bad_root, bad_commits) = project_with_root(&app, "jack/bad", 1);
    app.log
        .land(
            &app,
            landing_in(&app, "jack/bad", "main", None, &bad_commits[0]),
        )
        .expect("main");
    jam(&bad_root.dir());
    let stuck = app
        .log
        .land(
            &app,
            landing_in(&app, "jack/bad", "doomed", None, &bad_commits[0]),
        )
        .expect_err("stuck");
    assert_eq!(stuck.status, 503, "{}", stuck.message);
    let stamp = app.log.tip();
    drop(app);

    let app = App::open(config(&data)).expect("starts with a landing stuck");
    let stuck = app.log.stuck();
    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0].stamp, stamp);
    assert!(
        stuck[0].cause.contains("cannot lock references"),
        "{}",
        stuck[0].cause
    );
    let refused = app
        .log
        .land(
            &app,
            landing_in(&app, "jack/bad", "next", None, &bad_commits[0]),
        )
        .expect_err("still stuck");
    assert_eq!(refused.status, 503);
    assert!(
        refused.message.contains("cannot lock references"),
        "{}",
        refused.message
    );
    app.log
        .land(&app, landing(&app, "fine", None, &commits[0]))
        .expect("another project lands");

    // The operator gives it up: one entry appended, the log never edited.
    let tip = app.log.tip();
    let (void, voided) = app.log.void(&app, stamp).expect("voided");
    assert_eq!(void, tip + 1);
    assert_eq!(voided, vec![stamp]);
    assert!(app.log.stuck().is_empty());
    assert_eq!(app.log.applied(), app.log.tip());
    assert_eq!(support::root_of(&app, "jack/bad", "doomed"), None);
    assert_eq!(
        support::root_of(&app, "jack/bad", "main").as_deref(),
        Some(bad_commits[0].as_str())
    );
    {
        let db = app.db.lock();
        assert!(db::voided_at(&db, "jack/bad", stamp).expect("voided"));
        assert!(!db::voided_at(&db, "jack/bad", void).expect("after the void"));
        assert!(!db::voided_at(&db, HOME, stamp).expect("another project"));
    }
    let incarnation = support::incarnation(&app, "jack/bad");
    let gone = app
        .log
        .list_at("jack/bad", &incarnation, "doomed", void, false)
        .expect("read");
    assert_eq!(gone, None, "the void's state does not have it");
    unjam(&bad_root.dir());
    app.log
        .land(
            &app,
            landing_in(&app, "jack/bad", "next", None, &bad_commits[0]),
        )
        .expect("its project lands again");
    // Voiding what is in place is refused.
    let refused = app.log.void(&app, void).expect_err("not stuck");
    assert_eq!(refused.status, 409);
    drop(app);
    let app = App::open(config(&data)).expect("starts");
    assert!(app.log.stuck().is_empty());
    assert_eq!(app.log.applied(), app.log.tip());
    let _ = std::fs::remove_dir_all(&data);
}
