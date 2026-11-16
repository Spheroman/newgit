// A landing that changes a project's definitions bumps its epoch, written in
// the landing's entry and put in place with its rows (#411). A landing
// prepared under another epoch is refused under the lock, so its merge
// starts again; an entry from before epochs reads as bumping nothing.

mod support;

use newgit_serve::db;
use newgit_serve::landing::Entry;
use support::{HOME, server, tempdir};

fn epoch(app: &newgit_serve::App) -> u64 {
    db::epoch(&app.db.lock(), HOME).expect("epoch")
}

#[test]
fn a_landing_prepared_under_another_epoch_is_refused() {
    let data = tempdir("epochs");
    let (app, _root, commits) = server(&data, 4);
    app.log
        .land(&app, support::landing(&app, "main", None, &commits[0]))
        .expect("made");
    assert_eq!(epoch(&app), 0);

    // A definitions change: it bumps the epoch, under the epoch it read.
    let mut barrier = support::landing(&app, "main", Some(&commits[0]), &commits[1]);
    barrier.epochs = vec![(HOME.to_owned(), 0)];
    barrier.entry.homes.get_mut(HOME).expect("home").epoch = Some(1);
    app.log.land(&app, barrier).expect("lands");
    assert_eq!(epoch(&app), 1);

    // Prepared under epoch 0, on a branch that did not move: refused.
    let mut stale = support::landing(&app, "other", None, &commits[2]);
    stale.epochs = vec![(HOME.to_owned(), 0)];
    let refused = app.log.land(&app, stale).expect_err("an older epoch");
    assert_eq!(refused.status, 409, "{}", refused.message);
    assert!(
        refused.message.contains("start again"),
        "{}",
        refused.message
    );

    // A bump that skips one is refused too: it read the wrong epoch.
    let mut skips = support::landing(&app, "main", Some(&commits[1]), &commits[2]);
    skips.entry.homes.get_mut(HOME).expect("home").epoch = Some(3);
    assert_eq!(app.log.land(&app, skips).expect_err("skips").status, 409);

    // Under the current one, it lands.
    let mut current = support::landing(&app, "other", None, &commits[2]);
    current.epochs = vec![(HOME.to_owned(), 1)];
    app.log.land(&app, current).expect("lands");
    assert_eq!(
        epoch(&app),
        1,
        "a landing that changes no definitions bumps nothing"
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn an_entry_from_before_epochs_reads_as_bumping_nothing() {
    let entry: Entry = serde_json::from_str(
        r#"{"format":1,"by":"jack","homes":{"jack/p":{"incarnation":"x","branches":{}}}}"#,
    )
    .expect("an older entry reads");
    assert_eq!(entry.homes["jack/p"].epoch, None);
    let written = serde_json::to_string(&entry).expect("writes");
    assert!(!written.contains("epoch"), "{written}");
}
