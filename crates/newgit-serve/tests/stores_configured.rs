// A store made before its config said to fsync gets that config when the
// server starts (#435's second review), so a push into it, which runs with
// the store's own config, is as durable as a landing.

mod support;

use newgit_serve::App;
use support::{config, git, server, tempdir};

#[test]
fn an_existing_store_gets_the_fsync_config_at_startup() {
    let data = tempdir("stores-configured");
    let (app, root, _c) = server(&data, 1);
    drop(app);
    let store = root.dir();
    // As a store made before this server set them.
    git(&store, &["config", "--unset", "core.fsync"]);
    git(&store, &["config", "--unset", "core.fsyncMethod"]);
    git(&store, &["config", "receive.fsckObjects", "false"]);
    let app = App::open(config(&data)).expect("starts");
    assert_eq!(
        git(&store, &["config", "core.fsync"]),
        "committed,reference"
    );
    assert_eq!(git(&store, &["config", "core.fsyncMethod"]), "fsync");
    assert_eq!(git(&store, &["config", "receive.fsckObjects"]), "true");
    // Only what differs is written: a second start changes nothing.
    drop(app);
    let before = std::fs::read_to_string(store.join("config")).expect("config");
    let app = App::open(config(&data)).expect("starts again");
    assert_eq!(
        std::fs::read_to_string(store.join("config")).expect("config"),
        before
    );
    drop(app);
    let _ = std::fs::remove_dir_all(&data);
}
