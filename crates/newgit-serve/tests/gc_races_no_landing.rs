// Garbage collection (#434) never races a landing. A home is collected
// holding its root: a landing there meanwhile is answered 503, naming the
// collection, and one in another project lands. The landing log is collected
// holding the leader's seat: a landing meanwhile waits, writes nothing, and
// lands once the collection is done.

mod support;

use std::time::{Duration, Instant};

use camino::Utf8Path;
use newgit_serve::landing::is_busy;
use newgit_serve::maintenance;
use support::{HOME, git, landing, landing_in, project_with_root, server, tempdir};

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(60), "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run in a process of its own, with the fault file in its environment.
#[test]
fn collecting_never_races_a_landing() {
    let data = tempdir("gc-races-no-landing");
    let faults = data.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let output = std::process::Command::new(std::env::current_exe().expect("this test"))
        .args(["--exact", "in_its_own_process", "--ignored", "--nocapture"])
        .env("NEWGIT_SERVE_FAULT_FILE", faults.as_str())
        .env("GC_RACES_DATA", data.as_str())
        .output()
        .expect("runs");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&data);
}

fn held_at(faults: &Utf8Path, at: &str) -> bool {
    std::fs::read_to_string(format!("{faults}.at")).is_ok_and(|held| held == at)
}

#[test]
#[ignore = "run by collecting_never_races_a_landing, in its own process"]
fn in_its_own_process() {
    let data = camino::Utf8PathBuf::from(std::env::var("GC_RACES_DATA").expect("data"));
    let faults = data.join("faults");
    let (app, _root, commits) = server(&data.join("serve"), 3);
    let (_other, others) = project_with_root(&app, "ana/q", 1);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("lands");
    app.log.set_root_wait(Duration::from_millis(300));

    // A home, held while it is collected.
    std::fs::write(&faults, "hold-home-collect\n").expect("arm");
    std::thread::scope(|scope| {
        let collecting =
            scope.spawn(|| maintenance::collect_home_within(&app, HOME, Duration::ZERO));
        wait_for("the collection holds the home", || {
            held_at(&faults, "hold-home-collect")
        });
        let refused = app
            .log
            .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
            .expect_err("the home is held");
        assert!(is_busy(&refused), "{}", refused.message);
        assert!(
            refused.message.contains("garbage collection"),
            "{}",
            refused.message
        );
        app.log
            .land(&app, landing_in(&app, "ana/q", "main", None, &others[0]))
            .expect("another project lands meanwhile");
        std::fs::write(&faults, "").expect("disarm");
        collecting.join().expect("joined").expect("collected");
    });
    app.log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
        .expect("lands once the collection is done");

    // The log, collected with no batch writing.
    let log = data.join("serve/landing.git");
    let tip = app.log.tip();
    std::fs::write(&faults, "hold-log-collect\n").expect("arm");
    std::thread::scope(|scope| {
        let collecting = scope.spawn(|| app.log.collect());
        wait_for("the collection holds the log", || {
            held_at(&faults, "hold-log-collect")
        });
        let landing_now = scope.spawn(|| {
            app.log
                .land(&app, landing(&app, "main", Some(&commits[1]), &commits[2]))
        });
        std::thread::sleep(Duration::from_millis(800));
        assert!(!landing_now.is_finished(), "the landing waits for the log");
        assert_eq!(app.log.tip(), tip, "and writes nothing meanwhile");
        assert_eq!(
            git(&log, &["rev-list", "--count", "refs/heads/main"]),
            tip.to_string()
        );
        std::fs::write(&faults, "").expect("disarm");
        collecting
            .join()
            .expect("joined")
            .expect("the log collects");
        landing_now.join().expect("joined").expect("then it lands");
    });
    assert_eq!(app.log.tip(), tip + 1);
    assert_eq!(
        git(&log, &["rev-list", "--count", "refs/heads/main"]),
        (tip + 1).to_string()
    );
    let output = std::process::Command::new("git")
        .args(["--git-dir", log.as_str(), "fsck", "--no-dangling"])
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        support::root_of(&app, HOME, "main"),
        Some(commits[2].clone())
    );
}
