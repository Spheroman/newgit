// The background extension and fetches racing on one stored rooted history
// (#412's review, item 2): landings move root's `main` back and forth while
// the real background thread extends it and fetches bring it up to the
// lists they read (`rooted::as_of`, what a fetch and an export do). No fetch
// is ever answered with a history composed from lists older than the ones
// it read, and once answered, what is stored never moves back. Then the
// same race made certain: the background thread reads the lists, a landing
// and a fetch come and go before it writes, and what it writes then (from
// lists older than the fetch's) never replaces what the fetch served.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use newgit_serve::db;
use newgit_serve::names::parse_project_key;
use newgit_serve::rooted;
use support::{HOME, landing, server, tempdir};

#[test]
fn a_fetch_never_serves_a_view_behind_the_lists_it_read() {
    let data = tempdir("rooted-fetch-race");
    let (app, _root, commits) = server(&data, 5);
    let app = Arc::new(app);
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("lands");
    rooted::start(&app);
    let id = parse_project_key(HOME).expect("key");

    let stop = Arc::new(AtomicBool::new(false));
    let lander = {
        let (app, stop, commits) = (app.clone(), stop.clone(), commits.clone());
        std::thread::spawn(move || {
            let mut at = 0;
            for step in 1..=24 {
                let next = (step * 3) % commits.len();
                if next == at {
                    continue;
                }
                app.log
                    .land(
                        &app,
                        landing(&app, "main", Some(&commits[at]), &commits[next]),
                    )
                    .expect("lands");
                at = next;
                std::thread::sleep(Duration::from_millis(15));
            }
            stop.store(true, Ordering::SeqCst);
        })
    };

    let mut fetches = 0;
    let mut last = 0;
    while !stop.load(Ordering::SeqCst) || fetches < 5 {
        let (read, _) = db::views_at(&app.db.lock(), HOME).expect("revision");
        let history = rooted::as_of(&app, &id, "root", 0)
            .expect("answered")
            .expect("root has one");
        assert!(
            history.at.revision >= read,
            "served a view composed at revision {} after reading the lists at {read}",
            history.at.revision
        );
        assert!(
            history.at.revision >= last,
            "moved back from {last} to {}",
            history.at.revision
        );
        assert!(
            history
                .refs
                .iter()
                .any(|(name, _)| name == "refs/heads/main"),
            "{:?}",
            history.refs
        );
        last = history.at.revision;
        fetches += 1;
    }
    lander.join().expect("lander");
    // Once the background is done too, it is exactly the last lists.
    let (read, _) = db::views_at(&app.db.lock(), HOME).expect("revision");
    std::thread::sleep(Duration::from_millis(300));
    let history = rooted::as_of(&app, &id, "root", 0)
        .expect("answered")
        .expect("root has one");
    assert!(history.at.revision >= read);
    assert!(fetches >= 5);
    assert!(
        app.rooted.failing().is_empty(),
        "{:?}",
        app.rooted.failing()
    );
    let _ = std::fs::remove_dir_all(&data);
}

fn state_at(dir: &camino::Utf8Path) -> Option<String> {
    let output = newgit_core::git::command()
        .args([
            "--git-dir",
            dir.as_str(),
            "cat-file",
            "blob",
            "refs/newgit/rooted/root:served",
        ])
        .output()
        .expect("git");
    let served = String::from_utf8_lossy(&output.stdout).into_owned();
    output.status.success().then(|| {
        served
            .lines()
            .find(|line| line.starts_with("head main "))
            .unwrap_or_default()
            .to_owned()
    })
}

/// Run in a process of its own, with the fault file in its environment.
#[test]
fn a_worker_whose_lists_went_stale_writes_nothing_over_a_fetch() {
    let data = tempdir("rooted-stale-worker");
    let faults = data.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let output = std::process::Command::new(std::env::current_exe().expect("this test"))
        .args(["--exact", "stale_worker_in_its_own_process", "--ignored"])
        .env("NEWGIT_SERVE_FAULT_FILE", faults.as_str())
        .env("ROOTED_RACE_DATA", data.as_str())
        .output()
        .expect("runs");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
#[ignore = "run by a_worker_whose_lists_went_stale_writes_nothing_over_a_fetch"]
fn stale_worker_in_its_own_process() {
    let data = camino::Utf8PathBuf::from(std::env::var("ROOTED_RACE_DATA").expect("data"));
    let faults = data.join("faults");
    let (app, root, commits) = server(&data.join("serve"), 3);
    let app = Arc::new(app);
    let id = parse_project_key(HOME).expect("key");
    app.log
        .land(&app, landing(&app, "main", None, &commits[0]))
        .expect("lands");
    rooted::start(&app);
    let started = Instant::now();
    while !app.rooted.backfilled.load(Ordering::SeqCst)
        || app.rooted.passes.load(Ordering::SeqCst) < 2
    {
        assert!(started.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(20));
    }

    // The worker reads the lists at L1 and stops a moment before writing.
    std::fs::write(&faults, "rooted-after-snapshot\n").expect("arm");
    let passes = app.rooted.passes.load(Ordering::SeqCst);
    app.log
        .land(&app, landing(&app, "main", Some(&commits[0]), &commits[1]))
        .expect("L1 lands");
    std::thread::sleep(Duration::from_millis(400));
    // L2 lands and a fetch serves it before the worker writes.
    app.log
        .land(&app, landing(&app, "main", Some(&commits[1]), &commits[2]))
        .expect("L2 lands");
    let served = rooted::as_of(&app, &id, "root", 0)
        .expect("answered")
        .expect("root has one");
    let fetched = state_at(&root.dir()).expect("stored");
    assert!(fetched.contains(&commits[2]), "{fetched}");
    // The worker's stale pass ends; the next one (for L2) is held at the
    // same point, so this is what the stale pass left.
    while app.rooted.passes.load(Ordering::SeqCst) <= passes {
        assert!(started.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = state_at(&root.dir()).expect("stored");
    assert_eq!(after, fetched, "the stale worker moved it back");
    let again = rooted::as_of(&app, &id, "root", 0)
        .expect("answered")
        .expect("root has one");
    assert!(again.at.revision >= served.at.revision);
    std::fs::write(&faults, "").expect("disarm");
}
