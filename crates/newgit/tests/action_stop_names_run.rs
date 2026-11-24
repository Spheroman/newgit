//! `newgit action web.stop` names the run it stopped, `web.start`, as
//! `newgit stop` does, not the signal action itself.

mod evict_support;

use evict_support::*;

#[test]
fn a_signal_action_names_the_run_it_stopped() {
    let (_guard, temp) = tempdir("action-stop-names-run");
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("db.toml"),
        "ownership = \"branch\"\n\n[actions.start]\ncommand = \"sleep 1000\"\nlong_running = \
         true\n\n[actions.stop]\nsignal = \"int\"\n",
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "db.start", "x"]);
    let run = newgit_ok(&repo, &["action", "db.stop", "x"]);
    assert!(
        run.stdout.contains("Stopped `db.start` for `x` (pid "),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("`db.stop`"), "{}", run.stdout);
}
