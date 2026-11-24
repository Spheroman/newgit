// An entry of a newer format than this server reads refuses the start,
// whether or not it is in place (#435's second review): a newer server wrote
// it, and what it put in place may rely on it.

mod support;

use newgit_serve::App;
use newgit_serve::db;
use support::{config, git, landing, server, tempdir};

/// A log whose newest entry is format 2, appended after `landing x`;
/// returns its stamp.
fn newer_entry(data: &camino::Utf8Path) -> u64 {
    {
        let (app, _root, c) = server(data, 1);
        app.log
            .land(&app, landing(&app, "x", None, &c[0]))
            .expect("lands");
    }
    let log = data.join("landing.git");
    let tip = git(&log, &["rev-parse", "refs/heads/main"]);
    let tree = git(&log, &["rev-parse", "refs/heads/main^{tree}"]);
    let count: u64 = git(&log, &["rev-list", "--count", "refs/heads/main"])
        .parse()
        .expect("a count");
    let message = format!(
        "landing {}\n\n{{\"format\":2,\"by\":\"newer\"}}\n",
        count + 1
    );
    let newer = git(&log, &["commit-tree", &tree, "-p", &tip, "-m", &message]);
    git(&log, &["update-ref", "refs/heads/main", &newer]);
    count + 1
}

fn refusal(data: &camino::Utf8Path) -> String {
    match App::open(config(data)) {
        Ok(app) => panic!(
            "started: tip {}, applied {}",
            app.log.tip(),
            app.log.applied()
        ),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn a_newer_entry_not_in_place_refuses_the_start() {
    let data = tempdir("newer-format-unplaced");
    let stamp = newer_entry(&data);
    let refused = refusal(&data);
    assert!(
        refused.contains(&format!("landing {stamp} is entry format 2")),
        "{refused}"
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_newer_entry_below_the_applied_mark_refuses_the_start() {
    let data = tempdir("newer-format-placed");
    let stamp = newer_entry(&data);
    {
        let db = db::Db::open(&data.join("server.sqlite")).expect("opens");
        db::set_applied(&db.lock(), stamp).expect("marked");
    }
    let refused = refusal(&data);
    assert!(
        refused.contains(&format!("landing {stamp} is entry format 2")),
        "{refused}"
    );
    let _ = std::fs::remove_dir_all(&data);
}
