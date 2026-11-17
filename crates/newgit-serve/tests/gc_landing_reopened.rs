// A merge's landing garbage collection removed (#434) is made whole when it
// is opened again (#458's review, item 2): its `core.alternateRefsPrefixes`
// is written again, so a push into it is told of the tracker's branches as
// what the server has, and not of every ref in the store.

mod support;

use std::time::Duration;

use newgit_serve::maintenance;
use support::{HOME, git, server, tempdir};

#[test]
fn a_landing_opened_after_it_was_collected_has_its_prefix() {
    let data = tempdir("gc-landing-reopened");
    let (app, root, _) = server(&data, 1);
    let eve = root.open_landing("eve").expect("eve's landing");
    let prefix = git(&eve, &["config", "core.alternateRefsPrefixes"]);
    assert_eq!(prefix, root.full("refs/heads/"));

    let collected = maintenance::collect_home_within(&app, HOME, Duration::ZERO).expect("collects");
    assert_eq!(collected.landings_removed, 1, "{collected:?}");
    assert!(!eve.exists());

    let again = root.open_landing("eve").expect("opened again");
    assert_eq!(again, eve);
    assert_eq!(
        git(&again, &["config", "core.alternateRefsPrefixes"]),
        prefix,
        "written again"
    );
    let _ = std::fs::remove_dir_all(&data);
}
