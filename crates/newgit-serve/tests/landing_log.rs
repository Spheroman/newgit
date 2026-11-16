// The landing log (#410): every landing is one commit in `landing.git`, its
// stamp is its place there, refs and SQLite follow it, landings that arrive
// together are written as one batch (each still its own entry), "state at T"
// reads the log's tree, and a landing written down but not in place is put in
// place at the next start.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};

use newgit_serve::App;
use newgit_serve::db::{self, List};
use newgit_serve::landing::Stamp;
use support::{HOME, config, server, tempdir};

fn landing(
    app: &App,
    branch: &str,
    base: Option<&str>,
    commit: &str,
) -> newgit_serve::landing::Landing {
    support::landing(app, branch, base, commit)
}

fn root_of(app: &App, branch: &str) -> Option<String> {
    support::root_of(app, HOME, branch)
}

#[test]
fn landings_that_arrive_together_are_one_batch_each_its_own_entry() {
    let data = tempdir("log-batch");
    let (app, root, commits) = server(&data, 1);
    let app = Arc::new(app);
    let before = app.log.tip();
    let batches = app.log.batches();
    const LANDINGS: usize = 48;
    let start = Arc::new(Barrier::new(LANDINGS));
    let threads: Vec<_> = (0..LANDINGS)
        .map(|n| {
            let (app, start, commit) = (app.clone(), start.clone(), commits[0].clone());
            std::thread::spawn(move || {
                start.wait();
                app.log
                    .land(&app, landing(&app, &format!("b{n}"), None, &commit))
                    .expect("lands")
                    .stamp
            })
        })
        .collect();
    let mut stamps: Vec<Stamp> = threads
        .into_iter()
        .map(|thread| thread.join().expect("joined"))
        .collect();
    stamps.sort_unstable();
    let expected: Vec<Stamp> = (before + 1..=before + LANDINGS as Stamp).collect();
    assert_eq!(stamps, expected, "each landing is its own entry, in order");
    assert_eq!(app.log.tip(), before + LANDINGS as Stamp);
    let written = app.log.batches() - batches;
    assert!(
        written < LANDINGS as u64 / 2,
        "{LANDINGS} landings arriving together took {written} batches"
    );
    for n in 0..LANDINGS {
        let branch = format!("b{n}");
        assert_eq!(root_of(&app, &branch).as_deref(), Some(commits[0].as_str()));
        assert_eq!(
            root.branch_head(&branch).as_deref(),
            Some(commits[0].as_str())
        );
    }
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn state_at_a_stamp_is_read_from_the_log_and_a_stale_landing_is_refused() {
    let data = tempdir("log-state-at");
    let (app, root, commits) = server(&data, 3);
    let made = app
        .log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("made")
        .stamp;
    let moved = app
        .log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
        .expect("moved")
        .stamp;
    assert!(moved > made);
    let incarnation = support::incarnation(&app, HOME);
    let at = |stamp| {
        app.log
            .list_at(HOME, &incarnation, "main", stamp, false)
            .expect("read")
            .map(|list| list["root"].clone())
    };
    assert_eq!(at(made).as_deref(), Some(commits[0].as_str()));
    assert_eq!(at(moved).as_deref(), Some(commits[1].as_str()));
    assert_eq!(at(made - 1), None, "main did not exist yet");
    let ahead = app
        .log
        .list_at(HOME, &incarnation, "main", moved + 1, false)
        .expect_err("no landing has that stamp yet");
    assert_eq!(ahead.status, 409, "{}", ahead.message);
    {
        let db = app.db.lock();
        let branches = db::branches(&db, HOME).expect("branches");
        assert_eq!(branches[0]["stamp"], moved);
    }

    // Landed onto what main was: refused, and nothing written.
    let refused = app
        .log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[2]))
        .expect_err("main moved since");
    assert_eq!(refused.status, 409, "{}", refused.message);
    assert_eq!(app.log.tip(), moved);
    assert_eq!(
        root.branch_head("main").as_deref(),
        Some(commits[1].as_str())
    );
    // Made again: refused too.
    let refused = app
        .log
        .land(&app, landing(&app, "main", None, &commits[2]))
        .expect_err("main exists");
    assert_eq!(refused.status, 409, "{}", refused.message);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_landing_written_down_but_not_in_place_is_put_in_place_at_startup() {
    let data = tempdir("log-repair");
    let (app, root, commits) = server(&data, 2);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("made");
    let stamp = app
        .log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
        .expect("moved")
        .stamp;
    // As a crash between the log commit and everything after it leaves it:
    // the log has it, SQLite and the refs do not.
    {
        let db = app.db.lock();
        db::set_branch(
            &db,
            HOME,
            "main",
            &[("root".to_owned(), commits[0].clone())].into(),
            "jack",
            0,
        )
        .expect("back");
        db::set_applied(&db, stamp - 1).expect("not applied");
    }
    root.set_refs(&[("refs/heads/main".to_owned(), commits[0].clone())])
        .expect("ref back");
    drop(app);

    let app = App::open(config(&data)).expect("repairs");
    assert_eq!(root_of(&app, "main").as_deref(), Some(commits[1].as_str()));
    assert_eq!(
        root.branch_head("main").as_deref(),
        Some(commits[1].as_str())
    );
    let db = app.db.lock();
    assert_eq!(db::applied(&db).expect("applied"), Some(stamp));
    drop(db);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_data_directory_from_before_the_log_starts_it_from_sqlite() {
    let data = tempdir("log-genesis");
    let commit = "1111111111111111111111111111111111111111".to_owned();
    {
        let app = App::open(config(&data)).expect("opens");
        let db = app.db.lock();
        db::create_project(&db, HOME, "jack", "p").expect("project");
        // As a server from before the log wrote them, with no landing ever
        // put in place.
        let list: List = [("root".to_owned(), commit.clone())].into();
        db::set_branch(&db, HOME, "main", &list, "jack", 0).expect("main");
        db::set_tracker_store(&db, HOME, "root", "project-devs").expect("row");
        db.execute_batch("DELETE FROM landing_applied; DELETE FROM landings_placed")
            .expect("from before the log");
    }
    std::fs::remove_dir_all(data.join("landing.git")).expect("no log");
    let app = App::open(config(&data)).expect("opens");
    assert_eq!(app.log.tip(), 1);
    let incarnation = support::incarnation(&app, HOME);
    let first = app
        .log
        .list_at(HOME, &incarnation, "main", 1, false)
        .expect("read");
    assert_eq!(first, Some([("root".to_owned(), commit)].into()));
    let db = app.db.lock();
    assert_eq!(db::applied(&db).expect("applied"), Some(1));
    let _ = std::fs::remove_dir_all(&data);
}

/// Not a check: how many landings a second this machine writes when a
/// hundred arrive at once, each making a branch (one log entry, one ref move
/// in `project-devs.git`, one SQLite transaction). `cargo test -p
/// newgit-serve --release --test landing_log -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, run by hand"]
fn landings_per_second() {
    let data = tempdir("log-bench");
    let (app, _root, commits) = server(&data, 1);
    let app = Arc::new(app);
    const THREADS: usize = 100;
    const EACH: usize = 20;
    let batches = app.log.batches();
    let start = Arc::new(Barrier::new(THREADS));
    let began = std::time::Instant::now();
    let threads: Vec<_> = (0..THREADS)
        .map(|thread| {
            let (app, start, commit) = (app.clone(), start.clone(), commits[0].clone());
            std::thread::spawn(move || {
                start.wait();
                for n in 0..EACH {
                    app.log
                        .land(
                            &app,
                            landing(&app, &format!("t{thread}-{n}"), None, &commit),
                        )
                        .expect("lands");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("joined");
    }
    let took = began.elapsed();
    let landings = THREADS * EACH;
    let written = app.log.batches() - batches;
    let mut sizes = BTreeMap::new();
    sizes.insert("landings", landings as f64);
    sizes.insert("batches", written as f64);
    sizes.insert("seconds", took.as_secs_f64());
    sizes.insert("per_second", landings as f64 / took.as_secs_f64());
    println!("{sizes:?}");
    let _ = std::fs::remove_dir_all(&data);
}
