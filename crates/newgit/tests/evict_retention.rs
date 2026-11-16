//! Retention for what eviction leaves behind (#446): `newgit cleanup`
//! collects every `evict` checkpoint but the latest and the checkpoints a
//! restore that succeeded took on its way, with the store objects only they
//! reach, and says what it releases. An `explicit` checkpoint, the latest
//! `evict` one, and the one `remove` takes stay.

mod evict_support;

use camino::Utf8Path;
use evict_support::*;

/// `(id, reason)` per checkpoint `newgit checkpoints x` lists.
fn checkpoints(repo: &Utf8Path) -> Vec<(String, String)> {
    newgit_ok(repo, &["checkpoints", "x"])
        .stdout
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let id = words
                .next()
                .filter(|id| id.starts_with("ckpt_"))?
                .to_owned();
            let reason = words.nth(2)?.to_owned();
            Some((id, reason))
        })
        .collect()
}

fn reached(store: &Utf8Path) -> String {
    git_out(store, &["rev-list", "--all", "--objects"])
}

#[test]
fn cleanup_collects_superseded_evictions_and_what_their_restores_took() {
    let (_guard, temp) = tempdir("evict-retention");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    newgit_ok(&repo, &["checkpoint", "x", "-m", "mine"]);

    // A large untracked file round-trips through the first eviction's
    // dirty commit; it is gone by the second.
    let big = "big untracked content\n".repeat(4096);
    std::fs::write(ws.join("big.bin"), &big).expect("write");
    let blob = git_out(&ws, &["hash-object", "big.bin"]).trim().to_owned();
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(
        std::fs::read_to_string(ws.join("big.bin")).expect("back"),
        big
    );
    std::fs::remove_file(ws.join("big.bin")).expect("rm");
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    newgit_ok(&repo, &["start", "x"]);

    let before = checkpoints(&repo);
    let evicts: Vec<&String> = before
        .iter()
        .filter(|(_, reason)| reason == "evict")
        .map(|(id, _)| id)
        .collect();
    assert_eq!(evicts.len(), 2, "{before:?}");
    assert!(
        reached(&repo).contains(&blob),
        "the first eviction holds it"
    );

    let dry = newgit_ok(&repo, &["cleanup", "--dry-run"]);
    assert!(
        dry.stdout.contains("Would collect"),
        "a dry run says what it would collect:\n{}",
        dry.stdout
    );
    assert_eq!(checkpoints(&repo), before, "a dry run touches nothing");

    let run = newgit_ok(&repo, &["cleanup"]);
    assert!(
        run.stdout
            .contains(&format!("x {} (evict, superseded)", evicts[0])),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains(&format!("before-undo of the restore from {}", evicts[1])),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("released"), "{}", run.stdout);

    let after = checkpoints(&repo);
    assert_eq!(
        after,
        vec![
            (before[0].0.clone(), "explicit".to_owned()),
            (evicts[1].clone(), "evict".to_owned()),
        ],
        "only what was asked for and the latest eviction stay (was {before:?})"
    );
    assert!(
        !reached(&repo).contains(&blob),
        "no ref in the store reaches the file the first eviction held"
    );
    assert!(
        git_out(&repo, &["for-each-ref", "refs/newgit/checkpoints/x/"])
            .lines()
            .count()
            == 2,
        "the collected checkpoints' store refs are gone"
    );

    // Nothing more to collect, and undo still works to what stayed.
    let again = newgit_ok(&repo, &["cleanup"]);
    assert!(!again.stdout.contains("Collected"), "{}", again.stdout);
    newgit_ok(&repo, &["undo", "x", "--to", &before[0].0]);
}

#[test]
fn the_checkpoint_remove_takes_outlives_cleanup() {
    let (_guard, temp) = tempdir("evict-retention-remove");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    newgit_ok(&repo, &["stop", "--evict", "x"]);
    newgit_ok(&repo, &["start", "x"]);
    std::fs::write(ws.join("late.txt"), "late\n").expect("write");

    let removed = newgit_ok(&repo, &["remove", "x", "--json"]);
    let out: serde_json::Value = serde_json::from_str(&removed.stdout).expect("json");
    let id = out["checkpoint"].as_str().expect("checkpoint").to_owned();
    newgit_ok(&repo, &["cleanup"]);

    let dir = repo.join(".newgit/checkpoints/x");
    assert!(dir.join(format!("{id}.toml")).is_file(), "kept");
    newgit_ok(
        &repo,
        &["spawn", "x", "--from-checkpoint", &format!("x/{id}")],
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("late.txt")).expect("back"),
        "late\n"
    );
}
