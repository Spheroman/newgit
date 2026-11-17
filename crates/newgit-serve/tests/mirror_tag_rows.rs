// A tag made on a mirror, in the mirrors' rows (#453): its pull is its own.
//
// - A tag's pull and the branch pull of the commit it names are two pulls,
//   the tag's behind the branch's; the same tag event twice is one.
// - How a tag's pull ended is kept per tag, its latest. It lands nothing, so
//   it neither ends the branch's trouble nor is ended by a pull that lands:
//   a refused tag stays said until that tag is made.

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

fn queue(db: &Connection, commit: &str, tag: Option<&str>, delivery: &str) -> Queued {
    store::queue(
        db,
        HOME,
        "mirror",
        "jack/mirror",
        "main",
        commit,
        tag,
        "jack",
        Some(delivery),
        10,
    )
    .expect("queue")
}

fn pull(db: &Connection, commit: &str, tag: Option<&str>) -> Pull {
    store::pulls(db, HOME)
        .expect("pulls")
        .into_iter()
        .find(|pull| pull.commit == commit && pull.tag.as_deref() == tag)
        .expect("it is queued")
}

fn end(db: &Connection, commit: &str, tag: Option<&str>, outcome: &str, settles: bool) {
    let delivery = format!("d-{commit}-{tag:?}-{outcome}");
    queue(db, commit, tag, &delivery);
    let why = (outcome == "refused").then_some("why");
    store::end(
        db,
        &pull(db, commit, tag),
        &Ending {
            outcome,
            why,
            blocked_by: None,
            settles,
        },
        20,
    )
    .expect("end");
}

fn ended(db: &Connection) -> Vec<Value> {
    store::state(db, HOME, true, 1_000).expect("state")["ended"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn said(db: &Connection) -> Vec<(String, String)> {
    ended(db)
        .iter()
        .map(|one| {
            (
                one["tag"].as_str().unwrap_or("main").to_owned(),
                one["outcome"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[test]
fn a_tags_pull_is_its_own_and_waits_behind_its_commits() {
    let db = db();
    // A pull request merged, then tagged: two events naming one commit.
    let Queued::New(branch) = queue(&db, &commit(1), None, "d-branch") else {
        panic!("the branch's pull is queued");
    };
    let Queued::New(tag) = queue(&db, &commit(1), Some("v1"), "d-tag") else {
        panic!("the tag's pull is queued beside it");
    };
    assert!(branch < tag, "the tag's behind the branch's");
    // The same tag event again, and another tag of the same commit.
    assert_eq!(
        queue(&db, &commit(1), Some("v1"), "d-tag-again"),
        Queued::Already(tag)
    );
    assert!(matches!(
        queue(&db, &commit(1), Some("v2"), "d-tag-2"),
        Queued::New(_)
    ));
    let waiting = store::state(&db, HOME, true, 1_000).expect("state");
    assert_eq!(waiting["pulls"][0]["tag"], Value::Null, "{waiting}");
    assert_eq!(waiting["pulls"][1]["tag"], "v1", "{waiting}");
    // The thread takes the branch's first.
    assert_eq!(store::heads(&db).expect("heads")[0].id, branch);
}

#[test]
fn a_tag_neither_ends_the_branchs_trouble_nor_is_ended_with_it() {
    let db = db();
    end(&db, &commit(1), None, "refused", false);
    // A tag made meanwhile says nothing of the refused pull.
    end(&db, &commit(2), Some("v1"), "tagged", false);
    assert_eq!(
        said(&db),
        vec![
            ("v1".to_owned(), "tagged".to_owned()),
            ("main".to_owned(), "refused".to_owned()),
        ],
        "trouble is said last, so a reader does not take the tag for its end"
    );

    // A refused tag stays said when a pull lands, and when an export
    // reaches the mirror: neither makes the tag.
    end(&db, &commit(3), Some("v2"), "refused", false);
    end(&db, &commit(4), None, "landed", true);
    store::over(&db, HOME, "mirror").expect("over");
    assert_eq!(
        said(&db),
        vec![
            ("v1".to_owned(), "tagged".to_owned()),
            ("main".to_owned(), "landed".to_owned()),
            ("v2".to_owned(), "refused".to_owned()),
        ]
    );

    // Until the tag's own pull says it is made: one row per tag, its latest.
    end(&db, &commit(3), Some("v2"), "nothing", false);
    assert_eq!(
        said(&db),
        vec![
            ("v1".to_owned(), "tagged".to_owned()),
            ("main".to_owned(), "landed".to_owned()),
            ("v2".to_owned(), "nothing".to_owned()),
        ]
    );
}
