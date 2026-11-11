// Asked refs are compare-and-swap all the way down (#435's review, blocker
// 3). Within one batch, each landing is checked against the refs the ones
// before it move, so of many asking the same ref from the same old value one
// moves it and the rest are told it moved. Every ref change records its old
// value, and is put in place with it: a ref someone else moved since the
// check is left as they left it, never overwritten, after a restart too.

mod support;

use std::sync::{Arc, Barrier};

use newgit_serve::App;
use newgit_serve::db;
use newgit_serve::landing::{Asked, Landing};
use support::{HOME, config, landing, server, tempdir};

fn asking(app: &App, branch: &str, base: &str, reference: &str, new: &str) -> Landing {
    let mut asked = landing(app, branch, None, base);
    asked.asked.push(Asked {
        home: HOME.to_owned(),
        tracker: "root".to_owned(),
        reference: reference.to_owned(),
        old: None,
        new: new.to_owned(),
    });
    asked
}

#[test]
fn of_many_asking_one_ref_from_one_old_value_one_moves_it() {
    let data = tempdir("asked-batch");
    let (app, root, commits) = server(&data, 24);
    let app = Arc::new(app);
    let n = commits.len();
    let start = Arc::new(Barrier::new(n));
    let threads: Vec<_> = (0..n)
        .map(|i| {
            let (app, start, commits) = (app.clone(), start.clone(), commits.clone());
            std::thread::spawn(move || {
                let asked = asking(
                    &app,
                    &format!("b{i}"),
                    &commits[0],
                    "refs/heads/shared",
                    &commits[i],
                );
                start.wait();
                let landed = app.log.land(&app, asked).expect("lands");
                (commits[i].clone(), landed.behind.is_empty())
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("joined"))
        .collect();
    let moved: Vec<&String> = results
        .iter()
        .filter(|(_, moved)| *moved)
        .map(|(commit, _)| commit)
        .collect();
    assert_eq!(moved.len(), 1, "one is told it moved the ref: {moved:?}");
    assert_eq!(root.branch_head("shared").as_ref(), Some(moved[0]));
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn landings_queued_behind_a_batch_are_checked_against_each_other() {
    let data = tempdir("asked-queued");
    let (app, root, commits) = server(&data, 24);
    let app = Arc::new(app);
    for round in 0..5 {
        let reference = format!("refs/heads/shared{round}");
        // Keeps a leader busy so the rest queue up into one batch.
        let blocker = {
            let (app, commit) = (app.clone(), commits[0].clone());
            std::thread::spawn(move || {
                for k in 0..5 {
                    app.log
                        .land(
                            &app,
                            landing(&app, &format!("blk{round}-{k}"), None, &commit),
                        )
                        .expect("lands");
                }
            })
        };
        let threads: Vec<_> = (1..commits.len())
            .map(|i| {
                let (app, commits, reference) = (app.clone(), commits.clone(), reference.clone());
                std::thread::spawn(move || {
                    let asked = asking(
                        &app,
                        &format!("r{round}b{i}"),
                        &commits[0],
                        &reference,
                        &commits[i],
                    );
                    let landed = app.log.land(&app, asked).expect("lands");
                    (commits[i].clone(), landed.behind.is_empty())
                })
            })
            .collect();
        let moved: Vec<String> = threads
            .into_iter()
            .map(|thread| thread.join().expect("joined"))
            .filter(|(_, moved)| *moved)
            .map(|(commit, _)| commit)
            .collect();
        blocker.join().expect("joined");
        assert_eq!(moved.len(), 1, "round {round}: {moved:?}");
        assert_eq!(
            root.branch_head(reference.trim_start_matches("refs/heads/"))
                .as_ref(),
            Some(&moved[0])
        );
    }
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_ref_moved_by_someone_else_is_never_overwritten_by_a_replay() {
    let data = tempdir("asked-replay");
    let (app, root, commits) = server(&data, 3);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("main");
    let stamp = app
        .log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
        .expect("moved")
        .stamp;
    // As a crash before the rows and a push after it would leave it: the
    // log has the landing, SQLite does not, and the ref was pushed on.
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
        db.execute_batch("DELETE FROM landings_placed")
            .expect("unmarked");
        db::set_applied(&db, stamp - 1).expect("not applied");
    }
    root.set_refs(&[("refs/heads/main".to_owned(), commits[2].clone())])
        .expect("pushed on");
    drop(app);
    let app = App::open(config(&data)).expect("starts");
    assert_eq!(
        root.branch_head("main").as_deref(),
        Some(commits[2].as_str()),
        "the push is kept"
    );
    assert_eq!(
        support::root_of(&app, HOME, "main").as_deref(),
        Some(commits[1].as_str()),
        "the landing's list is in place"
    );
    assert!(app.log.stuck().is_empty());
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn every_ref_change_in_the_log_records_its_old_value() {
    let data = tempdir("asked-old");
    let (app, _root, commits) = server(&data, 2);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("main");
    app.log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
        .expect("moved");
    let message = support::git(
        &data.join("landing.git"),
        &["log", "-1", "--format=%B", "refs/heads/main"],
    );
    let body = message.split_once("\n\n").expect("an entry").1;
    let entry: serde_json::Value = serde_json::from_str(body).expect("JSON");
    assert_eq!(entry["format"], 1);
    let change = &entry["homes"][HOME];
    assert!(
        change["incarnation"]
            .as_str()
            .is_some_and(|it| !it.is_empty())
    );
    let moved = &change["refs"]["root"]["refs/heads/main"];
    assert_eq!(moved["old"], commits[0].as_str());
    assert_eq!(moved["new"], commits[1].as_str());
    let _ = std::fs::remove_dir_all(&data);
}
