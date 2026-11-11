// A void interrupted after it is written down never lets the landing it
// voids land (#435's second review, blocker 2, with the real binary).
//
// A landing making branch `doomed` is stuck (its store refuses ref writes).
// `newgit serve --void-landing <stamp>` writes the void down and is killed
// inside putting it in place, before its rows. At the next start the void is
// put in place and the landing it voids is not: the API and Git agree that
// `doomed` does not exist, and nothing is stuck.

#[path = "../../newgit-serve/tests/support/mod.rs"]
mod support;

use std::process::Command;

use camino::Utf8Path;
use newgit_serve::App;
use support::{config, jam, landing_in, project_with_root, root_of, server, tempdir, unjam};

fn reference(dir: &Utf8Path, name: &str) -> Option<String> {
    let output = Command::new("git")
        .args([
            "--git-dir",
            dir.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            name,
        ])
        .output()
        .expect("git runs");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[test]
fn a_void_killed_before_its_rows_never_lets_the_landing_it_voids_land() {
    let data = tempdir("void-interrupted");
    let (app, _root, _c) = server(&data, 1);
    let (bad_root, bad) = project_with_root(&app, "jack/bad", 1);
    app.log
        .land(&app, landing_in(&app, "jack/bad", "main", None, &bad[0]))
        .expect("main");
    jam(&bad_root.dir());
    app.log
        .land(&app, landing_in(&app, "jack/bad", "doomed", None, &bad[0]))
        .expect_err("stuck: its store refuses ref writes");
    let stamp = app.log.tip();
    drop(app);

    let faults = data.join("faults");
    std::fs::write(&faults, "before-rows\n").expect("faults");
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .args([
            "serve",
            "--data",
            data.as_str(),
            "--void-landing",
            &stamp.to_string(),
        ])
        .env("NEWGIT_SERVE_FAULT_FILE", faults.as_str())
        .output()
        .expect("runs");
    assert!(!output.status.success(), "killed inside the void");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("stopping at before-rows"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_file(&faults).expect("faults gone");
    unjam(&bad_root.dir());

    let app = App::open(config(&data)).expect("starts");
    assert_eq!(app.log.tip(), stamp + 1, "the void was written down");
    assert!(app.log.stuck().is_empty(), "{:?}", app.log.stuck());
    assert_eq!(app.log.applied(), stamp + 1);
    let doomed = bad_root.full("refs/heads/doomed");
    assert_eq!(
        root_of(&app, "jack/bad", "doomed"),
        None,
        "the API has no doomed"
    );
    assert_eq!(reference(&bad_root.dir(), &doomed), None, "nor has Git");
    assert_eq!(
        root_of(&app, "jack/bad", "main").as_deref(),
        Some(bad[0].as_str())
    );
    // `?at=` never shows the voided landing's state.
    let incarnation = support::incarnation(&app, "jack/bad");
    let voided = newgit_serve::db::voided_at(&app.db.lock(), "jack/bad", stamp).expect("reads");
    assert!(voided, "the voided landing's state is known to be voided");
    let at = app
        .log
        .list_at("jack/bad", &incarnation, "doomed", stamp + 1, false)
        .expect("in place");
    assert_eq!(at, None, "doomed is not in the void's state");
    // It lands again.
    app.log
        .land(&app, landing_in(&app, "jack/bad", "after", None, &bad[0]))
        .expect("lands");
    let _ = std::fs::remove_dir_all(&data);
}
