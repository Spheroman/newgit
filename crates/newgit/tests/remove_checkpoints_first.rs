//! `newgit remove` checkpoints the workspace before deleting it, so removing
//! never deletes work no checkpoint holds (#448): uncommitted, untracked,
//! and staged work comes back with `spawn --from-checkpoint`, the archived
//! record names the checkpoint, and what no checkpoint holds is named as
//! lost, in text and in `--json`.

mod evict_support;

use evict_support::*;
use serde_json::Value;

/// A project whose one resource sleeps, with an instance `x`.
fn spawned() -> (TempDir, camino::Utf8PathBuf, camino::Utf8PathBuf) {
    spawned_with(&[])
}

/// [`spawned`], with more resources: `(name, definition)`.
fn spawned_with(resources: &[(&str, &str)]) -> (TempDir, camino::Utf8PathBuf, camino::Utf8PathBuf) {
    let (guard, temp) = tempdir("remove-first");
    let store = setup(&temp);
    for (name, definition) in resources {
        std::fs::write(
            store.paths().resources.join(format!("{name}.toml")),
            definition,
        )
        .expect("write resource");
    }
    let root = store.paths().project_root.clone();
    newgit_ok(&root, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    (guard, root, ws)
}

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}):\n{}\nstderr:\n{}",
            run.stdout, run.stderr
        )
    })
}

#[test]
fn remove_keeps_dirty_work_in_a_checkpoint_spawn_brings_back() {
    let (_guard, root, ws) = spawned();
    std::fs::write(ws.join("user.txt"), "untracked work\n").expect("write");
    std::fs::write(ws.join("README.md"), "hello\nedited\n").expect("write");
    std::fs::write(ws.join("staged.txt"), "staged\n").expect("write");
    git(&ws, &["add", "staged.txt"]);
    std::fs::create_dir_all(ws.join("target")).expect("mkdir");
    std::fs::write(ws.join("target/bin"), "built\n").expect("write");

    let run = newgit_ok(&root, &["remove", "x", "--json"]);
    let out = json(&run);
    let checkpoint = out["checkpoint"]
        .as_str()
        .expect("a checkpoint id")
        .to_owned();
    assert!(!ws.join("user.txt").exists(), "the workspace is gone");
    let lost: Vec<&str> = out["lost"]
        .as_array()
        .expect("lost")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        lost.iter().any(|what| what.contains("`target/` (1 file)")),
        "ignored build output is named as lost: {lost:?}"
    );
    assert!(
        !lost.iter().any(|what| what.contains("user.txt")),
        "untracked work is in the checkpoint, not lost: {lost:?}"
    );
    assert!(out["unsaved_workspace_head"].is_null());

    let archived = std::fs::read_to_string(out["archived_record"].as_str().expect("record"))
        .expect("archived record");
    assert!(
        archived.contains(&format!("removed_to = \"{checkpoint}\"")),
        "the archived record names the checkpoint:\n{archived}"
    );
    let record =
        std::fs::read_to_string(root.join(format!(".newgit/checkpoints/x/{checkpoint}.toml")))
            .expect("the checkpoint record");
    assert!(
        record.contains("reason = \"remove\""),
        "recorded with reason `remove`:\n{record}"
    );

    newgit_ok(
        &root,
        &[
            "spawn",
            "x",
            "--from-checkpoint",
            &format!("x/{checkpoint}"),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("user.txt")).expect("back"),
        "untracked work\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("README.md")).expect("back"),
        "hello\nedited\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("staged.txt")).expect("back"),
        "staged\n"
    );
}

#[test]
fn remove_says_what_it_kept_and_what_no_checkpoint_holds() {
    let (_guard, root, ws) = spawned();
    std::fs::write(ws.join("README.md"), "stashed\n").expect("write");
    git(&ws, &["stash"]);
    git(&ws, &["checkout", "-q", "-b", "try-x"]);
    std::fs::write(ws.join("try.txt"), "x\n").expect("write");
    git(&ws, &["add", "try.txt"]);
    git(&ws, &["commit", "-q", "-m", "try"]);
    git(&ws, &["checkout", "-q", "x"]);

    let run = newgit_ok(&root, &["remove", "x"]);
    assert!(
        run.stdout.contains("kept:      checkpoint ckpt_"),
        "says which checkpoint holds the workspace:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("--from-checkpoint x/ckpt_"),
        "and how to bring it back:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("lost:      1 stash entry"),
        "the stash is named as lost:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("branch `try-x`"),
        "a branch only the workspace has is named as lost:\n{}",
        run.stdout
    );
}

#[test]
fn purge_takes_no_checkpoint_and_names_the_uncommitted_work_it_deletes() {
    let (_guard, root, ws) = spawned();
    std::fs::write(ws.join("user.txt"), "untracked work\n").expect("write");

    let out = json(&newgit_ok(&root, &["remove", "x", "--purge", "--json"]));
    assert!(out["checkpoint"].is_null(), "{out}");
    let lost = out["lost"].to_string();
    assert!(
        lost.contains("uncommitted work in `user.txt`"),
        "names what it deleted: {lost}"
    );
}

#[test]
fn a_checkpoint_that_fails_refuses_the_removal_unless_forced() {
    let (_guard, temp) = tempdir("remove-first-fails");
    let root = setup(&temp).paths().project_root.clone();
    newgit_ok(&root, &["tracker", "create", "scratch"]);
    newgit_ok(&root, &["tracker", "track", "scratch", "scratch"]);
    newgit_ok(&root, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    std::fs::write(ws.join("user.txt"), "untracked work\n").expect("write");
    // A tracker's capture refuses a link that leaves the workspace, so the
    // checkpoint fails.
    std::fs::create_dir_all(ws.join("scratch")).expect("mkdir");
    std::os::unix::fs::symlink("/etc/hosts", ws.join("scratch/out")).expect("symlink");

    let run = refused(
        &root,
        &["remove", "x"],
        "the checkpoint that keeps its workspace failed",
    );
    assert!(run.stderr.contains("--force"), "names the way through");
    assert!(ws.join("user.txt").exists(), "nothing was deleted");

    let out = json(&newgit_ok(&root, &["remove", "x", "--force", "--json"]));
    assert!(out["checkpoint"].is_null(), "{out}");
    assert!(
        out["lost"].to_string().contains("user.txt"),
        "names what it deleted: {out}"
    );
    assert!(!ws.exists(), "removed");
}
