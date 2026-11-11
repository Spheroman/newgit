// A damaged landing log refuses to start, loudly (#435's review, blocker
// 4). The log is made only when it did not exist before this start; a log
// that cannot be read, a missing log with landings put in place from it, and
// a log that ends before what SQLite says is in place are all refused, never
// taken for "a data directory from before the log". The log and the stores
// sync their refs.

mod support;

use newgit_serve::App;
use newgit_serve::db;
use support::{config, landing, server, tempdir};

fn refused(data: &camino::Utf8Path) -> String {
    match App::open(config(data)) {
        Ok(app) => panic!(
            "started on a damaged log at tip {} (applied {})",
            app.log.tip(),
            app.log.applied()
        ),
        Err(error) => format!("{error:#}"),
    }
}

/// A server with `main` landed twice, the second written down and not in
/// place; returns its data directory.
fn with_unapplied(name: &str) -> camino::Utf8PathBuf {
    let data = tempdir(name);
    let (app, _root, commits) = server(&data, 2);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("main");
    let stamp = app
        .log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
        .expect("moved")
        .stamp;
    {
        let db = app.db.lock();
        db.execute_batch("DELETE FROM landings_placed")
            .expect("unmarked");
        db::set_applied(&db, stamp - 1).expect("not applied");
    }
    data
}

#[test]
fn a_log_whose_refs_are_gone_refuses_to_start() {
    let data = with_unapplied("damaged-reftable");
    let log = data.join("landing.git");
    for entry in std::fs::read_dir(log.join("reftable")).expect("reftable") {
        let path = entry.expect("entry").path();
        if path.file_name().is_some_and(|name| name != "tables.list") {
            std::fs::remove_file(path).expect("remove");
        }
    }
    std::fs::write(log.join("reftable/tables.list"), "").expect("emptied");
    let error = refused(&data);
    assert!(
        error.contains("landing log") && error.contains("refusing to start"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_log_whose_objects_are_gone_refuses_to_start() {
    let data = with_unapplied("damaged-objects");
    let log = data.join("landing.git");
    // Every commit but the newest: loose or packed.
    let tip = support::git(&log, &["rev-parse", "refs/heads/main"]);
    for entry in std::fs::read_dir(log.join("objects")).expect("objects") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if name == "pack" {
            std::fs::remove_dir_all(&path).expect("remove");
            std::fs::create_dir(&path).expect("mkdir");
        } else if name.len() == 2 && name != tip[..2] {
            std::fs::remove_dir_all(&path).expect("remove");
        }
    }
    let error = refused(&data);
    assert!(error.contains("refusing to start"), "{error}");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_missing_log_with_landings_put_in_place_refuses_to_start() {
    let data = with_unapplied("damaged-missing");
    std::fs::remove_dir_all(data.join("landing.git")).expect("remove");
    let error = refused(&data);
    assert!(error.contains("is missing"), "{error}");
    assert!(
        !data.join("landing.git").exists(),
        "nothing is made over it"
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn sqlite_ahead_of_the_log_refuses_to_start() {
    let data = with_unapplied("damaged-ahead");
    {
        let app = App::open(config(&data)).expect("starts");
        let tip = app.log.tip();
        db::set_applied(&app.db.lock(), tip + 5).expect("ahead");
    }
    let error = refused(&data);
    assert!(error.contains("ends at"), "{error}");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_second_server_on_one_data_directory_is_refused() {
    let data = tempdir("damaged-two");
    let app = App::open(config(&data)).expect("starts");
    let error = refused(&data);
    assert!(error.contains("another newgit serve"), "{error}");
    drop(app);
    App::open(config(&data)).expect("starts once the first stopped");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn the_log_and_the_stores_sync_their_refs() {
    let data = tempdir("damaged-fsync");
    let (_app, root, _commits) = server(&data, 1);
    for dir in [data.join("landing.git"), root.dir()] {
        assert_eq!(
            support::git(&dir, &["config", "core.fsync"]),
            "committed,reference",
            "{dir}"
        );
    }
    let _ = std::fs::remove_dir_all(&data);
}
