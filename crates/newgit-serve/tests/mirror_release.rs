// A refused pull that lands while the mirror holds more (#451): the pulls
// refused only for carrying its commits are said on their own, not dropped
// with the refusal they waited on. And a pull's ending is written whole or
// not at all.

use newgit_serve::mirror::store::{self, Ending, Pull};
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

fn queued(db: &Connection, commit: &str, now: i64) -> Pull {
    store::queue(
        db,
        HOME,
        "mirror",
        "jack/mirror",
        "main",
        commit,
        None,
        "jack",
        Some(&format!("d-{commit}-{now}")),
        now,
    )
    .expect("queue");
    store::pulls(db, HOME)
        .expect("pulls")
        .into_iter()
        .find(|pull| pull.commit == commit)
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

fn ended(db: &Connection) -> Vec<Value> {
    store::state(db, HOME, true, 1_000).expect("state")["ended"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// A refused at 10, B (carrying A) waiting on it at 20.
fn a_refused_b_waiting(db: &Connection) -> (String, String) {
    let (a, b) = (commit(1), commit(2));
    let pull = queued(db, &a, 10);
    store::end(
        db,
        &pull,
        &ending("refused", Some("worker's work"), false),
        10,
    )
    .expect("end");
    let pull = queued(db, &b, 20);
    store::end(
        db,
        &pull,
        &Ending {
            blocked_by: Some(&a),
            ..ending("refused", Some("worker's work"), false)
        },
        20,
    )
    .expect("end");
    let said = ended(db);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0]["why"]
            .as_str()
            .unwrap_or_default()
            .contains("1 later pull")
    );
    (a, b)
}

#[test]
fn a_landing_that_leaves_more_on_the_mirror_says_what_waited_on_it() {
    let db = db();
    let (a, b) = a_refused_b_waiting(&db);
    // A sent again lands; the mirror still holds B.
    let pull = queued(&db, &a, 30);
    store::atomically(&db, |db| {
        store::release(
            db,
            HOME,
            "mirror",
            std::slice::from_ref(&a),
            "A landed: redeliver B",
        )?;
        store::end(db, &pull, &ending("landed", None, false), 30)
    })
    .expect("end");
    let said = ended(&db);
    assert_eq!(said.len(), 1, "A's refusal is over, B's is not: {said:?}");
    assert_eq!(said[0]["commit"], b.as_str());
    assert_eq!(said[0]["outcome"], "refused");
    assert_eq!(said[0]["why"], "A landed: redeliver B");

    // B sent again lands, and settles the target.
    let pull = queued(&db, &b, 40);
    store::end(&db, &pull, &ending("landed", None, true), 40).expect("end");
    let said = ended(&db);
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0]["outcome"], "landed");
}

#[test]
fn a_landing_that_settles_ends_everything() {
    let db = db();
    let (a, _) = a_refused_b_waiting(&db);
    let pull = queued(&db, &a, 30);
    store::end(&db, &pull, &ending("landed", None, true), 30).expect("end");
    let said = ended(&db);
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0]["outcome"], "landed");
}

#[test]
fn an_ending_is_written_whole_or_not_at_all() {
    let db = db();
    let (a, _) = a_refused_b_waiting(&db);
    let pull = queued(&db, &a, 30);
    let before = ended(&db);
    let failed: Result<(), _> = store::atomically(&db, |db| {
        store::end(db, &pull, &ending("landed", None, true), 30)?;
        Err(newgit_serve::error::HttpError::internal("the disk filled"))
    });
    assert!(failed.is_err());
    assert_eq!(ended(&db), before, "nothing it wrote stays");
    assert_eq!(
        store::pulls(&db, HOME).expect("pulls").len(),
        1,
        "still queued"
    );
}
