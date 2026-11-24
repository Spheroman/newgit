//! A workspace deleted by hand (`rm -rf`) comes back with `newgit start`:
//! workspaces are disposable and the binding record is not. `status` names
//! the command; `start` stops what still runs from the deleted directory,
//! then rebuilds from the instance's latest checkpoint, or from its source
//! branch when it has none, and says what did not come back.

mod evict_support;

use camino::{Utf8Path, Utf8PathBuf};
use evict_support::*;
use serde_json::Value;

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}):\n{}\n{}", run.stdout, run.stderr))
}

/// A project whose `db` runs `sleep` and whose `deps` writes `deps/marker`
/// in `prepare`, and an instance `x` with `db` running.
fn spawned(tag: &str) -> (TempDir, Utf8PathBuf, Utf8PathBuf) {
    let (guard, temp) = tempdir(tag);
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::write(repo.join(".gitignore"), "target/\ndeps/\n").expect("write");
    git(&repo, &["commit", "-qam", "ignore deps"]);
    std::fs::write(
        store.paths().resources.join("deps.toml"),
        "ownership = \"workspace\"\n\n[actions.prepare]\ncommand = \"mkdir -p deps && echo \
         prepared > deps/marker\"\n",
    )
    .expect("write resource");
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["action", "db.start", "x"]);
    (guard, repo, temp.join("workspaces/x"))
}

fn pid(repo: &Utf8Path) -> u32 {
    std::fs::read_to_string(repo.join(".newgit/state/x/db.pid"))
        .expect("pid file")
        .trim()
        .parse()
        .expect("a pid")
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn start_rebuilds_from_the_latest_checkpoint() {
    let (_guard, repo, ws) = spawned("rebuild-checkpoint");
    std::fs::write(ws.join("notes.txt"), "kept\n").expect("write");
    newgit_ok(&repo, &["checkpoint", "x", "-m", "notes"]);
    std::fs::write(ws.join("later.txt"), "never checkpointed\n").expect("write");
    let before = pid(&repo);
    std::fs::remove_dir_all(&ws).expect("rm -rf");

    let status = newgit_ok(&repo, &["status"]);
    assert!(
        status.stdout.contains("ws-missing")
            && status.stdout.contains("`newgit start x`")
            && status.stdout.contains("ckpt_001"),
        "{}",
        status.stdout
    );
    let listed = json(&newgit_ok(&repo, &["status", "--json"]));
    assert!(
        listed["instances"][0]["problems"][0]
            .as_str()
            .is_some_and(|problem| problem.contains("`newgit start x`")),
        "{listed}"
    );

    let out = json(&newgit_ok(&repo, &["start", "x", "--json"]));
    assert_eq!(out["rebuilt"]["checkpoint"], "ckpt_001", "{out}");
    assert!(
        out["warnings"][0]
            .as_str()
            .is_some_and(|warning| warning.contains("did not come back")),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("notes.txt")).expect("notes back"),
        "kept\n"
    );
    assert!(!ws.join("later.txt").exists(), "never checkpointed");
    // The process from the deleted directory is gone, and one runs in the
    // new workspace.
    assert!(
        !alive(before),
        "pid {before} still runs from the deleted directory"
    );
    let after = pid(&repo);
    assert_ne!(after, before);
    assert!(alive(after));
    let status = newgit_ok(&repo, &["status"]);
    assert!(!status.stdout.contains("ws-missing"), "{}", status.stdout);
}

#[test]
fn start_rebuilds_from_the_source_branch_without_a_checkpoint() {
    let (_guard, repo, ws) = spawned("rebuild-branch");
    // A commit made in the workspace reaches the store only with a
    // checkpoint, so with none it goes with the workspace.
    std::fs::write(ws.join("local.txt"), "only in the workspace\n").expect("write");
    git(&ws, &["add", "local.txt"]);
    git(&ws, &["commit", "-qm", "local"]);
    std::fs::write(ws.join("dirty.txt"), "uncommitted\n").expect("write");
    newgit_ok(&repo, &["stop", "x"]);
    std::fs::remove_dir_all(&ws).expect("rm -rf");

    let status = newgit_ok(&repo, &["status"]);
    assert!(
        status
            .stdout
            .contains("from branch `x`, since it has no checkpoint"),
        "{}",
        status.stdout
    );
    let run = newgit_ok(&repo, &["start", "x"]);
    assert!(
        run.stdout
            .contains("Rebuilt the deleted workspace of `x` from its source branch"),
        "{}",
        run.stdout
    );
    assert!(run.stderr.contains("did not come back"), "{}", run.stderr);
    assert!(ws.join("README.md").exists(), "the branch's content");
    assert!(!ws.join("local.txt").exists());
    assert!(!ws.join("dirty.txt").exists());
    // Resources bound again, `prepare` included, and what `stop` stopped
    // started again.
    assert_eq!(
        std::fs::read_to_string(ws.join("deps/marker")).expect("prepared again"),
        "prepared\n"
    );
    assert!(run.stdout.contains("Started `db.start`"), "{}", run.stdout);
    assert!(alive(pid(&repo)));
}

#[test]
fn undo_on_a_deleted_workspace_names_start() {
    let (_guard, repo, ws) = spawned("rebuild-undo-hint");
    newgit_ok(&repo, &["checkpoint", "x"]);
    std::fs::remove_dir_all(&ws).expect("rm -rf");
    refused(&repo, &["undo", "x"], "`newgit start x` rebuilds it");
}
