//! `{{snapshot.path}}` has a value only when `[checkpoint] into_tracker`
//! names the tracker its contents go into. Without one the command ran with
//! the literal text, and `newgit resource check` passed it.

mod evict_support;

use evict_support::*;

fn check(tag: &str, checkpoint: &str) -> Run {
    let (_guard, temp) = tempdir(tag);
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!("ownership = \"branch\"\n\n[checkpoint]\nmode = \"command\"\n{checkpoint}"),
    )
    .expect("write resource");
    newgit(&repo, &["resource", "check"])
}

#[test]
fn a_snapshot_path_with_no_tracker_to_go_into_fails_the_check() {
    let run = check(
        "snapshot-path-no-tracker",
        "command = \"true > {{snapshot.path}}/dump.sql\"\n",
    );
    assert!(!run.status.success(), "{}{}", run.stdout, run.stderr);
    let said = format!("{}{}", run.stdout, run.stderr);
    assert!(
        said.contains("{{snapshot.path}}") && said.contains("into_tracker"),
        "{said}"
    );
}

#[test]
fn a_command_that_does_not_use_it_passes_without_one() {
    let run = check("snapshot-path-unused", "command = \"true\"\n");
    assert!(run.status.success(), "{}{}", run.stdout, run.stderr);
}
