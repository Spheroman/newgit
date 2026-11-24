//! `newgit remove` never runs the `[cleanup]` of a resource its checkpoint
//! could not capture (second review of #465): a failing `pg_dump` followed
//! by a `dropdb` would delete the only copy of the database, unattended.
//! The state stays where it is, and the output says so, where, and how to
//! remove it; `--force` runs the cleanup, and says what that deleted.

mod evict_support;

use camino::Utf8PathBuf;
use evict_support::*;
use serde_json::Value;

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}):\n{}\n{}", run.stdout, run.stderr))
}

/// A project whose resource `db` keeps its state at `db-data/` outside the
/// workspace, whose dump always fails, and whose `[cleanup]` deletes it;
/// and an instance `x`.
fn spawned(tag: &str) -> (TempDir, Utf8PathBuf, Utf8PathBuf) {
    let (guard, temp) = tempdir(tag);
    let store = setup(&temp);
    let db = temp.join("db-data");
    std::fs::create_dir_all(&db).expect("mkdir");
    std::fs::write(db.join("rows"), "precious\n").expect("write");
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!(
            "ownership = \"branch\"\n\n[checkpoint]\nmode = \"command\"\ncommand = \"exit \
             3\"\n\n[cleanup]\ncommand = \"rm -rf {db}\"\n"
        ),
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    (guard, repo, db)
}

#[test]
fn a_plain_remove_leaves_state_its_capture_missed() {
    let (_guard, repo, db) = spawned("remove-uncaptured-left");
    let run = newgit_ok(&repo, &["remove", "x"]);
    assert!(
        db.join("rows").exists(),
        "dropped the database whose dump failed:\n{}",
        run.stdout
    );
    // Said plainly: left, where (its cleanup names it), and how to remove it.
    assert!(
        run.stdout
            .contains("left:      resource `db`'s state, left where it is")
            && run.stdout.contains(&format!("`rm -rf {db}`"))
            && run.stdout.contains("--force"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("cleanup:  db SKIPPED"),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("lost:      the state of resource"),
        "{}",
        run.stdout
    );
    // The checkpoint went ahead; nothing says it was aborted.
    assert!(
        run.stdout.contains("kept:      checkpoint"),
        "{}",
        run.stdout
    );
    assert!(!run.stderr.contains("aborted"), "{}", run.stderr);
}

#[test]
fn json_names_what_was_left_and_the_hook_it_skipped() {
    let (_guard, repo, db) = spawned("remove-uncaptured-json");
    let out = json(&newgit_ok(&repo, &["remove", "x", "--json"]));
    assert!(db.join("rows").exists(), "{out}");
    let left = out["left"].as_array().expect("left");
    assert_eq!(left.len(), 1, "{out}");
    assert_eq!(left[0]["resource"], "db", "{out}");
    assert_eq!(left[0]["cleanup"], format!("rm -rf {db}"), "{out}");
    assert!(
        left[0]["reason"]
            .as_str()
            .is_some_and(|why| why.contains("exited with 3") && !why.contains("aborted")),
        "{out}"
    );
    let hook = out["hooks"]
        .as_array()
        .expect("hooks")
        .iter()
        .find(|hook| hook["resource"] == "db")
        .expect("db's hook");
    assert_eq!(hook["status"], "skipped-uncaptured", "{out}");
    assert!(out["checkpoint"].is_string(), "{out}");
    assert!(
        out["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .all(|warning| !warning.as_str().unwrap_or("").contains("aborted")),
        "{out}"
    );
}

#[test]
fn force_runs_the_cleanup_and_names_what_it_deleted() {
    let (_guard, repo, db) = spawned("remove-uncaptured-force");
    let out = json(&newgit_ok(&repo, &["remove", "x", "--force", "--json"]));
    assert!(!db.exists(), "--force runs the cleanup: {out}");
    assert!(out["left"].as_array().expect("left").is_empty(), "{out}");
    assert!(
        out["lost"].as_array().expect("lost").iter().any(|what| {
            what.as_str().is_some_and(|what| {
                what.contains("resource `db`") && what.contains("ran anyway (--force)")
            })
        }),
        "{out}"
    );
}
