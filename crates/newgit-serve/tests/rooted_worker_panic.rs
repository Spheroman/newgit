// A panic in the background extension (#412's review, item 4) is caught,
// said, and counted as a failure; the thread keeps going, and the failure is
// tried again with backoff without waiting for another landing there.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use support::{landing, server, tempdir};

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(60), "{what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Run in a process of its own, with the fault file and a short backoff in
/// its environment: what this test binary does below.
#[test]
fn a_panic_in_the_extension_stops_nothing() {
    let data = tempdir("rooted-worker-panic");
    let faults = data.join("faults");
    std::fs::write(&faults, "rooted-panic\n").expect("arm");
    let output = std::process::Command::new(std::env::current_exe().expect("this test"))
        .args(["--exact", "in_its_own_process", "--ignored", "--nocapture"])
        .env("NEWGIT_SERVE_FAULT_FILE", faults.as_str())
        .env("NEWGIT_SERVE_ROOTED_RETRY_MS", "50")
        .env("ROOTED_PANIC_DATA", data.as_str())
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{stderr}"
    );
    assert!(stderr.contains("panicked"), "the panic is said: {stderr}");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
#[ignore = "run by a_panic_in_the_extension_stops_nothing, in its own process"]
fn in_its_own_process() {
    let data = camino::Utf8PathBuf::from(std::env::var("ROOTED_PANIC_DATA").expect("data"));
    let faults = data.join("faults");
    let (app, root, commits) = server(&data.join("serve"), 2);
    let app = Arc::new(app);
    app.log
        .land(&app, landing(&app, "main", None, &commits[1]))
        .expect("lands");
    newgit_serve::rooted::start(&app);
    wait_for("a pass panics", || {
        app.rooted.failed.load(Ordering::SeqCst) >= 2 && !app.rooted.failing().is_empty()
    });
    let failing = app.rooted.failing();
    assert!(
        failing[0].error.contains("panicked"),
        "{:?}",
        failing[0].error
    );
    assert_eq!(
        app.rooted.refreshed.load(Ordering::SeqCst),
        0,
        "failures are not counted"
    );
    let state = || {
        newgit_core::git::command()
            .args([
                "--git-dir",
                root.dir().as_str(),
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/newgit/rooted/root",
            ])
            .output()
            .expect("git")
            .status
            .success()
    };
    assert!(!state());

    // Disarmed, with nothing landing: tried again, and done.
    std::fs::write(&faults, "").expect("disarm");
    wait_for("the retry extends it", || {
        state() && app.rooted.failing().is_empty()
    });
    assert!(app.rooted.refreshed.load(Ordering::SeqCst) >= 1);

    // And the thread is still there for the next landing.
    let passes = app.rooted.passes.load(Ordering::SeqCst);
    app.log
        .land(&app, landing(&app, "main", Some(&commits[1]), &commits[0]))
        .expect("lands");
    wait_for("the next landing is extended", || {
        app.rooted.passes.load(Ordering::SeqCst) > passes
    });
}
