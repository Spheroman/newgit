// The mirrors' rows in SQLite (#414, #451): the pull queue, and how pulls
// that ended are kept for `newgit status`.
//
// - An event is queued once while it waits (by commit and target, and by
//   delivery), and again once its pull has ended: a redelivery asks again.
// - The queue is rows, so a restart finds it as it was (#353).
// - #451 (c): a refusal is said once, with how many later pulls wait on it,
//   and is never pushed out of what is kept by those pulls, nor by another
//   target's. A redelivery refused again is said once.
// - Trouble ends when a pull lands or finds the mirror with nothing `main`
//   lacks, or an export reaches the mirror: not when an old delivery sent
//   again finds its own commit on `main`.
// - A pull refused for a reason of its own takes the place of the refusals
//   whose commits it carries.

use newgit_serve::mirror::store::{self, Ending, Pull, Queued};
use rusqlite::Connection;
use serde_json::Value;

const HOME: &str = "jack/store";

fn db() -> Connection {
    let db = Connection::open_in_memory().expect("sqlite");
    store::create(&db).expect("tables");
    db
}

fn commit(n: u32) -> String {
    format!("{n:040x}")
}

/// Queue a pull of `commit` to `target`, as its event would.
fn queued(db: &Connection, target: &str, commit: &str, now: i64) -> Pull {
    let delivery = format!("d-{target}-{commit}-{now}");
    store::queue(
        db,
        HOME,
        target,
        "jack/mirror",
        "main",
        commit,
        None,
        "jack",
        Some(&delivery),
        now,
    )
    .expect("queue");
    store::pulls(db, HOME)
        .expect("pulls")
        .into_iter()
        .find(|pull| pull.target == target && pull.commit == commit)
        .expect("it is queued")
}

fn ending<'a>(outcome: &'a str, why: Option<&'a str>, settles: bool) -> Ending<'a> {
    Ending {
        outcome,
        why,
        blocked_by: None,
        settles,
    }
}

/// A pull of `commit` to `target` that ended refused, for `why`.
fn refused(db: &Connection, target: &str, commit: &str, why: &str, now: i64) {
    let pull = queued(db, target, commit, now);
    store::end(db, &pull, &ending("refused", Some(why), false), now).expect("end");
}

/// A pull of `commit` refused only for carrying `first`.
fn waiting_on(db: &Connection, target: &str, commit: &str, first: &str, now: i64) {
    let pull = queued(db, target, commit, now);
    store::end(
        db,
        &pull,
        &Ending {
            blocked_by: Some(first),
            ..ending("refused", Some("the same reason"), false)
        },
        now,
    )
    .expect("end");
}

fn ended(db: &Connection) -> Vec<Value> {
    store::state(db, HOME, true, 1_000).expect("state")["ended"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn an_event_is_queued_once_while_it_waits_and_again_once_it_ended() {
    let db = db();
    let queue = |target: &str, commit: &str, delivery: &str| {
        store::queue(
            &db,
            HOME,
            target,
            "jack/mirror",
            "main",
            commit,
            None,
            "jack",
            Some(delivery),
            10,
        )
        .expect("queue")
    };
    let Queued::New(first) = queue("mirror", &commit(1), "d-1") else {
        panic!("the first event is queued");
    };
    // The same delivery again (a redelivery while it waits), and the same
    // commit under another delivery (GitHub sending one push twice).
    assert_eq!(queue("mirror", &commit(1), "d-1"), Queued::Already(first));
    assert_eq!(queue("mirror", &commit(1), "d-2"), Queued::Already(first));
    // Another commit, and the same commit to another target, are their own.
    assert!(matches!(queue("mirror", &commit(2), "d-3"), Queued::New(_)));
    assert!(matches!(queue("t3code", &commit(1), "d-4"), Queued::New(_)));
    assert_eq!(store::pulls(&db, HOME).expect("pulls").len(), 3);
    // Another project's queue is its own, whatever the delivery says.
    let elsewhere = store::queue(
        &db,
        "jack/other",
        "mirror",
        "jack/mirror",
        "main",
        &commit(1),
        None,
        "jack",
        Some("d-1"),
        10,
    )
    .expect("queue");
    assert!(matches!(elsewhere, Queued::New(_)));

    // Once its pull has ended, the delivery sent again is queued again.
    let pull = store::pulls(&db, HOME)
        .expect("pulls")
        .into_iter()
        .find(|pull| pull.id == first)
        .expect("the first");
    store::end(&db, &pull, &ending("refused", Some("why"), false), 20).expect("end");
    assert!(matches!(queue("mirror", &commit(1), "d-1"), Queued::New(_)));
}

#[test]
fn the_queue_is_rows_a_restart_finds_as_it_left_them() {
    let dir =
        std::env::temp_dir().join(format!("newgit-serve-mirror-queue-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("server.sqlite");
    let (first, second) = {
        let db = Connection::open(&path).expect("sqlite");
        store::create(&db).expect("tables");
        let first = queued(&db, "mirror", &commit(1), 10);
        let second = queued(&db, "mirror", &commit(2), 11);
        // The first failed once and waits; it was asking GitHub.
        assert_eq!(
            store::failed(&db, first.id, "GitHub is down", 99, 12).expect("failed"),
            1
        );
        store::set_lookup(
            &db,
            first.id,
            &store::Lookup {
                since: Some(12),
                last: 13,
                failed: Some("GitHub answered 502".to_owned()),
                failures: 2,
            },
            13,
        )
        .expect("lookup");
        (first.id, second.id)
    };

    // The server stopped and started: the same file, opened again.
    let db = Connection::open(&path).expect("sqlite");
    store::create(&db).expect("tables are made only once");
    let pulls = store::pulls(&db, HOME).expect("pulls");
    assert_eq!(
        pulls.iter().map(|pull| pull.id).collect::<Vec<_>>(),
        vec![first, second],
        "in the order they came"
    );
    let waiting = &pulls[0];
    assert_eq!(waiting.attempts, 1);
    assert_eq!(waiting.last_error.as_deref(), Some("GitHub is down"));
    assert_eq!(waiting.retry_at, Some(99));
    let lookup = waiting.lookup.clone().expect("what GitHub was asked");
    assert_eq!(lookup.failures, 2);
    assert_eq!(lookup.since, Some(12));
    // And the thread takes up the oldest of each project.
    let heads = store::heads(&db).expect("heads");
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, first);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refusal_is_never_pushed_out_by_the_pulls_waiting_on_it() {
    let db = db();
    refused(
        &db,
        "mirror",
        &commit(1),
        "worker's work would be left out",
        10,
    );
    // More later pulls than are kept one by one, each sent twice.
    for n in 2..=30 {
        waiting_on(&db, "mirror", &commit(n), &commit(1), 10 + i64::from(n));
        waiting_on(&db, "mirror", &commit(n), &commit(1), 50 + i64::from(n));
    }
    let said = ended(&db);
    assert_eq!(said.len(), 1, "one refusal, said once: {said:?}");
    assert_eq!(said[0]["commit"], commit(1));
    let why = said[0]["why"].as_str().unwrap_or_default();
    assert!(why.starts_with("worker's work would be left out"), "{why}");
    assert!(
        why.contains("29 later pulls"),
        "each later pull counted once, however often it was sent: {why}"
    );
    assert!(why.contains(&commit(30)[..12]), "the latest named: {why}");
}

#[test]
fn another_targets_pulls_never_push_a_refusal_out() {
    let db = db();
    refused(&db, "mirror", &commit(1), "refused here", 10);
    // The project's other mirror goes on being exported to and pulled from.
    for n in 0..25 {
        let pull = queued(&db, "t3code", &commit(100 + n), 20 + i64::from(n));
        store::end(
            &db,
            &pull,
            &ending("nothing", None, true),
            20 + i64::from(n),
        )
        .expect("end");
        let pull = queued(&db, "t3code", &commit(200 + n), 20 + i64::from(n));
        store::end(&db, &pull, &ending("landed", None, true), 20 + i64::from(n)).expect("end");
    }
    let said = ended(&db);
    let refusals: Vec<&Value> = said
        .iter()
        .filter(|one| one["outcome"] == "refused")
        .collect();
    assert_eq!(refusals.len(), 1, "{said:?}");
    assert_eq!(refusals[0]["target"], "mirror");
    // And a target that only ever settles keeps one row, its last.
    let settled: Vec<&Value> = said
        .iter()
        .filter(|one| one["target"] == "t3code")
        .collect();
    assert_eq!(settled.len(), 1, "{said:?}");
    assert_eq!(settled[0]["outcome"], "landed");
}

#[test]
fn a_redelivery_refused_again_is_said_once() {
    let db = db();
    refused(&db, "mirror", &commit(1), "refused", 10);
    waiting_on(&db, "mirror", &commit(2), &commit(1), 11);
    refused(&db, "mirror", &commit(1), "refused", 12);
    refused(
        &db,
        "mirror",
        &commit(1),
        "refused, for another reason now",
        13,
    );
    let said = ended(&db);
    assert_eq!(said.len(), 1, "{said:?}");
    let why = said[0]["why"].as_str().unwrap_or_default();
    assert!(why.starts_with("refused, for another reason now"), "{why}");
    assert!(
        why.contains("1 later pull "),
        "the pull that waited on it before it was sent again still does: {why}"
    );
}

#[test]
fn only_a_pull_that_settles_the_target_ends_its_trouble() {
    let db = db();
    refused(&db, "mirror", &commit(5), "refused", 10);
    // An old delivery sent again: its commit is long on `main`, and the
    // mirror still holds the refused one. It says nothing of the trouble.
    let old = queued(&db, "mirror", &commit(1), 11);
    store::end(&db, &old, &ending("nothing", None, false), 11).expect("end");
    assert!(
        store::pulls(&db, HOME).expect("pulls").is_empty(),
        "it is off the queue all the same"
    );
    let said = ended(&db);
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0]["outcome"], "refused");

    // A pull that finds the mirror with nothing `main` lacks ends it.
    let new = queued(&db, "mirror", &commit(6), 12);
    store::end(&db, &new, &ending("nothing", None, true), 12).expect("end");
    let said = ended(&db);
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0]["outcome"], "nothing");

    // With no trouble to hide, an old delivery's `nothing` is what is kept.
    let old = queued(&db, "mirror", &commit(1), 13);
    store::end(&db, &old, &ending("nothing", None, false), 13).expect("end");
    let said = ended(&db);
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0]["commit"], commit(1));
}

#[test]
fn an_export_that_reaches_the_mirror_ends_its_trouble() {
    let db = db();
    refused(&db, "mirror", &commit(1), "refused", 10);
    waiting_on(&db, "mirror", &commit(2), &commit(1), 11);
    let gave_up = queued(&db, "mirror", &commit(3), 12);
    store::end(
        &db,
        &gave_up,
        &ending("gave up", Some("ten times"), false),
        12,
    )
    .expect("end");
    refused(&db, "t3code", &commit(4), "refused there", 13);

    store::over(&db, HOME, "mirror").expect("over");
    let said = ended(&db);
    assert_eq!(said.len(), 1, "the other target's stays said: {said:?}");
    assert_eq!(said[0]["target"], "t3code");
}

#[test]
fn a_refusal_of_its_own_takes_the_place_of_those_it_carries() {
    let db = db();
    refused(&db, "mirror", &commit(1), "run `newgit merge worker`", 10);
    waiting_on(&db, "mirror", &commit(2), &commit(1), 11);
    refused(&db, "mirror", &commit(9), "unrelated: not carried", 12);
    // The later pull, sent again once `worker` merged, now conflicts: it
    // would land the first's commits too, and its reason is the one left.
    store::supersede(&db, HOME, "mirror", &[commit(1), commit(7)]).expect("supersede");
    refused(&db, "mirror", &commit(2), "it conflicts in a.rs", 13);
    let said = ended(&db);
    let whys: Vec<&str> = said
        .iter()
        .map(|one| one["why"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        whys,
        vec!["unrelated: not carried", "it conflicts in a.rs"],
        "advice already taken is not said again"
    );
}
