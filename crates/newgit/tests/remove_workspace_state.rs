//! `newgit remove` never deletes state that lives in the workspace when
//! its capture failed: a Postgres whose data directory is in the tree and
//! whose dump failed went with the workspace, while `remove` exited 0 and
//! said it was "left in place". State outside the workspace is still left
//! where it is (`remove_uncaptured_state.rs`). And `newgit checkpoints`
//! lists the checkpoints a removal points at.

mod evict_support;

use camino::Utf8PathBuf;
use evict_support::*;
use serde_json::Value;

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}):\n{}\n{}", run.stdout, run.stderr))
}

/// A project whose resource `db` keeps its data at `pgdata/` in the
/// workspace (ignored), with `ownership` and `checkpoint` as given, and an
/// instance `x`. Returns the project root and the data file.
fn spawned(tag: &str, ownership: &str, checkpoint: &str) -> (TempDir, Utf8PathBuf, Utf8PathBuf) {
    let (guard, temp) = tempdir(tag);
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::write(repo.join(".gitignore"), "target/\npgdata/\n").expect("write");
    git(&repo, &["commit", "-qam", "ignore pgdata"]);
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!(
            "ownership = \"{ownership}\"\n\n[actions.prepare]\ncommand = \"mkdir -p pgdata && \
             echo precious > pgdata/rows\"\n\n[checkpoint]\nmode = \"command\"\ncommand = \
             \"{checkpoint}\"\n\n[cleanup]\ncommand = \"rm -rf pgdata\"\n"
        ),
    )
    .expect("write resource");
    newgit_ok(&repo, &["spawn", "x"]);
    let rows = temp.join("workspaces/x/pgdata/rows");
    assert!(rows.exists(), "prepare made the data");
    (guard, repo, rows)
}

#[test]
fn a_failed_capture_of_state_in_the_workspace_refuses_the_removal() {
    let (_guard, repo, rows) = spawned("remove-ws-state-refuses", "workspace", "exit 3");
    let run = refused(&repo, &["remove", "x"], "not removing `x`");
    assert!(
        run.stderr.contains("`db`")
            && run.stderr.contains("ownership = \"workspace\"")
            && run.stderr.contains("Nothing was stopped or deleted")
            && run.stderr.contains("newgit remove x --force"),
        "{}",
        run.stderr
    );
    assert!(rows.exists(), "deleted the data whose dump failed");
    // Still an instance: nothing was archived.
    let status = newgit_ok(&repo, &["status", "x"]);
    assert!(status.stdout.contains(" x "), "{}", status.stdout);
}

#[test]
fn force_removes_it_and_names_the_state_as_lost() {
    let (_guard, repo, rows) = spawned("remove-ws-state-force", "workspace", "exit 3");
    let out = json(&newgit_ok(&repo, &["remove", "x", "--force", "--json"]));
    assert!(!rows.exists(), "{out}");
    assert!(out["left"].as_array().expect("left").is_empty(), "{out}");
    assert!(
        out["lost"].as_array().expect("lost").iter().any(|what| {
            what.as_str().is_some_and(|what| {
                what.contains("resource `db`")
                    && what.contains("in the workspace")
                    && what.contains("(--force)")
            })
        }),
        "{out}"
    );
}

#[test]
fn state_outside_the_workspace_is_left_and_says_why_it_is_outside() {
    let (_guard, repo, _rows) = spawned("remove-ws-state-branch", "branch", "exit 3");
    let run = newgit_ok(&repo, &["remove", "x"]);
    assert!(
        run.stdout.contains(
            "left where it is (outside the workspace, as its `ownership = \"branch\"` says)"
        ),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("left in place"), "{}", run.stdout);
}

#[test]
fn ignored_data_a_dump_holds_is_not_called_unkept() {
    let (_guard, repo, _rows) = spawned("remove-ws-state-dumped", "workspace", "cat pgdata/rows");
    let run = newgit_ok(&repo, &["remove", "x"]);
    let line = run
        .stdout
        .lines()
        .find(|line| line.contains("`pgdata/`"))
        .unwrap_or_else(|| panic!("pgdata named:\n{}", run.stdout));
    assert!(
        line.contains("what `db` captured of its own state") && line.contains("ckpt_001"),
        "{line}"
    );
    assert!(!line.contains("rebuilt by no resource"), "{line}");
}

#[test]
fn checkpoints_lists_what_a_removal_points_at() {
    let (_guard, repo, _rows) = spawned("remove-ws-state-list", "workspace", "cat pgdata/rows");
    let run = newgit_ok(&repo, &["remove", "x"]);
    assert!(
        run.stdout.contains("--from-checkpoint x/ckpt_001"),
        "{}",
        run.stdout
    );
    let listed = newgit_ok(&repo, &["checkpoints", "x"]);
    assert!(
        listed.stdout.contains("`x` was removed")
            && listed.stdout.contains("ckpt_001")
            && listed.stdout.contains("--from-checkpoint x/<id>"),
        "{}",
        listed.stdout
    );
    assert!(
        !listed.stdout.contains("newgit undo x"),
        "{}",
        listed.stdout
    );
    refused(&repo, &["checkpoints", "nobody"], "nobody");
}
