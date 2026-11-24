// `?at=<stamp>` answers the stamp's own landing, and only landings in place
// (#435's review, blocker 5): it resolves the log's commit once and reads
// that, so a landing written meanwhile never shifts it, and a stamp above
// the applied mark is refused.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use support::{HOME, landing, server, tempdir};

#[test]
fn a_stamp_read_while_landings_are_written_is_that_stamp() {
    let data = tempdir("at-race");
    let (app, _root, commits) = server(&data, 1);
    let app = Arc::new(app);
    let incarnation = support::incarnation(&app, HOME);
    let stop = Arc::new(AtomicBool::new(false));
    let first = app.log.tip() + 1;
    let writer = {
        let (app, stop, commit) = (app.clone(), stop.clone(), commits[0].clone());
        std::thread::spawn(move || {
            for n in 0..300 {
                app.log
                    .land(&app, landing(&app, &format!("w{n}"), None, &commit))
                    .expect("lands");
            }
            stop.store(true, Ordering::SeqCst);
        })
    };
    let (mut wrong, mut tries) = (0, 0);
    while !stop.load(Ordering::SeqCst) {
        let at = app.log.applied();
        if at < first {
            continue;
        }
        // w{k} landed at stamp first + k.
        let k = at - first;
        tries += 1;
        let made = app
            .log
            .list_at(HOME, &incarnation, &format!("w{k}"), at, false)
            .expect("in place");
        let next = app
            .log
            .list_at(HOME, &incarnation, &format!("w{}", k + 1), at, false)
            .expect("in place");
        if made.is_none() || next.is_some() {
            wrong += 1;
        }
    }
    writer.join().expect("joined");
    assert!(tries > 0);
    assert_eq!(wrong, 0, "{wrong} of {tries} reads showed another stamp");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_stamp_not_in_place_yet_is_refused() {
    let data = tempdir("at-ahead");
    let (app, _root, commits) = server(&data, 1);
    let stamp = app
        .log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("main")
        .stamp;
    let incarnation = support::incarnation(&app, HOME);
    let refused = app
        .log
        .list_at(HOME, &incarnation, "main", stamp + 1, false)
        .expect_err("not written");
    assert_eq!(refused.status, 409, "{}", refused.message);
    let voided = app
        .log
        .list_at(HOME, &incarnation, "main", stamp, true)
        .expect_err("voided since");
    assert_eq!(voided.status, 410, "{}", voided.message);
    let _ = std::fs::remove_dir_all(&data);
}
