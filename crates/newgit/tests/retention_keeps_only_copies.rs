//! `newgit cleanup` collects a checkpoint only when it is proven to be
//! nobody's only copy (#446, review of #465): an `evict` checkpoint nothing
//! restored stays, even once a later instance with the same name shares its
//! history and evicts past it; cleanup deletes refs and records, never the
//! store's objects; and a checkpoint ref no record names is collected
//! without its id being handed out again.

mod evict_support;

use camino::Utf8Path;
use evict_support::*;

/// The first `evict` checkpoint `newgit checkpoints x` lists.
fn first_evict(repo: &Utf8Path) -> String {
    newgit_ok(repo, &["checkpoints", "x"])
        .stdout
        .lines()
        .find(|line| line.contains("evict"))
        .and_then(|line| line.split_whitespace().next())
        .expect("an evict checkpoint")
        .to_owned()
}

#[test]
fn an_evicted_then_removed_instance_keeps_its_work_when_its_name_is_reused() {
    let (_guard, temp) = tempdir("retention-reuse");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    std::fs::write(temp.join("workspaces/x/work.txt"), "only copy\n").expect("write");
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    let first = first_evict(&repo);
    newgit_ok(&repo, &["remove", "x"]);

    // The same name again, evicted: its checkpoint is the history's latest.
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    let cleanup = newgit_ok(&repo, &["cleanup"]);
    assert!(
        !cleanup.stdout.contains(&first),
        "never restored, so never collected:\n{}",
        cleanup.stdout
    );

    newgit_ok(
        &repo,
        &["spawn", "y", "--from-checkpoint", &format!("x/{first}")],
    );
    assert_eq!(
        std::fs::read_to_string(temp.join("workspaces/y/work.txt")).ok(),
        Some("only copy\n".to_owned())
    );
}

/// Every loose object in `git_dir` dated in 2000: far past the hour a
/// prune of the store's unreachable objects would have spared.
fn age_loose_objects(git_dir: &Utf8Path) {
    let objects = git_dir.join("objects");
    for dir in std::fs::read_dir(&objects).expect("objects").flatten() {
        if dir.file_name().len() != 2 {
            continue;
        }
        for file in std::fs::read_dir(dir.path()).expect("fan-out").flatten() {
            let status = std::process::Command::new("touch")
                .args(["-t", "200001010000"])
                .arg(file.path())
                .status()
                .expect("touch");
            assert!(status.success());
        }
    }
}

#[test]
fn cleanup_deletes_refs_and_records_but_never_prunes_the_store() {
    let (_guard, temp) = tempdir("retention-no-prune");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    std::fs::write(ws.join("held.txt"), "held by the first eviction\n").expect("write");
    let blob = git_out(&ws, &["hash-object", "held.txt"]).trim().to_owned();
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    newgit_ok(&repo, &["start", "x"]);
    std::fs::remove_file(ws.join("held.txt")).expect("rm");
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    newgit_ok(&repo, &["start", "x"]);
    let first = first_evict(&repo);
    age_loose_objects(&repo.join(".git"));

    let run = newgit_ok(&repo, &["cleanup"]);
    assert!(
        std::process::Command::new("git")
            .args(["-C", repo.as_str(), "cat-file", "-e", &blob])
            .status()
            .expect("git")
            .success(),
        "the object itself is left for the store's own gc"
    );
    assert!(
        run.stdout
            .contains(&format!("x {first} (evict, restored and superseded)")),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("the store's own `git gc`"),
        "says who frees the bytes:\n{}",
        run.stdout
    );
    assert!(
        !git_out(&repo, &["rev-list", "--all", "--objects"]).contains(&blob),
        "no ref reaches it any more"
    );
}

#[test]
fn a_checkpoint_ref_no_record_names_is_collected_and_its_id_not_reused() {
    let (_guard, temp) = tempdir("retention-orphan");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["checkpoint", "x", "-m", "mine"]);
    // What a checkpoint that failed between its ref and its record left.
    let head = git_out(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    git(
        &repo,
        &["update-ref", "refs/newgit/checkpoints/x/ckpt_007", &head],
    );
    newgit_ok(&repo, &["cleanup"]);
    assert!(
        git_out(
            &repo,
            &["for-each-ref", "refs/newgit/checkpoints/x/ckpt_007"]
        )
        .is_empty(),
        "the orphan ref is gone"
    );
    assert!(
        !git_out(
            &repo,
            &["for-each-ref", "refs/newgit/checkpoints/x/ckpt_001"]
        )
        .is_empty(),
        "a recorded checkpoint's ref stays"
    );

    // A collected record's id stays taken.
    let ws = temp.join("workspaces/x");
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    newgit_ok(&repo, &["start", "x"]);
    std::fs::write(ws.join("late.txt"), "late\n").expect("write");
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    newgit_ok(&repo, &["start", "x"]);
    let listed = newgit_ok(&repo, &["checkpoints", "x"]).stdout;
    let last = listed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|id| id.starts_with("ckpt_"))
        .max()
        .expect("checkpoints")
        .to_owned();
    newgit_ok(&repo, &["cleanup"]);
    newgit_ok(&repo, &["checkpoint", "x", "-m", "after"]);
    let after = newgit_ok(&repo, &["checkpoints", "x"]).stdout;
    let newest = after
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|id| id.starts_with("ckpt_"))
        .max()
        .expect("checkpoints")
        .to_owned();
    assert!(
        newest > last,
        "a new checkpoint never reuses a collected id: {newest} after {last}\n{after}"
    );
}
