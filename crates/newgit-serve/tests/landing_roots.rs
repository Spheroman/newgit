// Roots' locks (#435's review, items 6, 9, 10).
//
// - A landing names every home it touches before it waits: one that asks a
//   ref in a linked tracker's home waits on that home too.
// - Roots held ahead of a landing (a merge placing its commits) keep every
//   other landing there out until they are let go of, so no store move can
//   land between deciding where a merge's commits go and putting them there.
// - A landing that waits too long for a root another holds is answered 503,
//   saying what holds it, rather than hanging.

mod support;

use std::sync::Arc;
use std::time::Duration;

use newgit_serve::landing::{Asked, Change};
use support::{HOME, landing, make_project, server, tempdir};

fn roots(homes: &[&str]) -> std::collections::BTreeSet<String> {
    homes.iter().map(|home| (*home).to_owned()).collect()
}

#[test]
fn roots_held_keep_every_other_landing_there_out() {
    let data = tempdir("roots-hold");
    let (app, root, commits) = server(&data, 1);
    let app = Arc::new(app);
    let hold = app
        .log
        .hold(&app, roots(&[HOME]), "a merge is placing its commits")
        .expect("held");
    let tip = app.log.tip();
    let waiting = {
        let (app, commit) = (app.clone(), commits[0].clone());
        std::thread::spawn(move || {
            app.log
                .land(&app, landing(&app, "main", None, &commit))
                .map(|landed| landed.stamp)
        })
    };
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        app.log.tip(),
        tip,
        "nothing landed while the roots were held"
    );
    assert_eq!(root.branch_head("main"), None);
    drop(hold);
    let stamp = waiting.join().expect("joined").expect("lands once let go");
    assert_eq!(stamp, tip + 1);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_landing_waiting_too_long_is_answered_busy_with_why() {
    let data = tempdir("roots-busy");
    let (app, _root, commits) = server(&data, 1);
    app.log.set_root_wait(Duration::from_millis(300));
    let hold = app
        .log
        .hold(&app, roots(&[HOME]), "waiting for a push into site to end")
        .expect("held");
    let busy = app
        .log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect_err("busy");
    assert_eq!(busy.status, 503, "{}", busy.message);
    assert!(
        busy.message.contains("waiting for a push into site to end"),
        "{}",
        busy.message
    );
    drop(hold);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("lands once let go");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_ref_asked_in_another_home_takes_that_home_too() {
    let data = tempdir("roots-linked");
    let (app, _root, commits) = server(&data, 1);
    let other = make_project(&app, "jack/lib");
    app.log.set_root_wait(Duration::from_millis(300));
    let hold = app
        .log
        .hold(&app, roots(&["jack/lib"]), "jack/lib is landing")
        .expect("held");
    let mut asked = landing(&app, "main", None, &commits[0]);
    asked.entry.homes.insert(
        "jack/lib".to_owned(),
        Change {
            incarnation: other,
            ..Change::default()
        },
    );
    asked.asked.push(Asked {
        home: "jack/lib".to_owned(),
        tracker: "root".to_owned(),
        reference: "refs/heads/main".to_owned(),
        old: None,
        new: commits[0].clone(),
    });
    let busy = app.log.land(&app, asked).expect_err("its home is held");
    assert_eq!(busy.status, 503, "{}", busy.message);
    assert!(busy.message.contains("jack/lib"), "{}", busy.message);
    drop(hold);

    // A ref asked in a home the entry does not name is never landed.
    let mut unnamed = landing(&app, "main", None, &commits[0]);
    unnamed.asked.push(Asked {
        home: "jack/lib".to_owned(),
        tracker: "root".to_owned(),
        reference: "refs/heads/main".to_owned(),
        old: None,
        new: commits[0].clone(),
    });
    let refused = app.log.land(&app, unnamed).expect_err("unnamed home");
    assert_eq!(refused.status, 500);
    let _ = std::fs::remove_dir_all(&data);
}
