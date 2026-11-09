//! A push learns the view commits it shares with the remote's `main` by
//! composing that `main` in the project root. Once it is composed at the
//! remote's root commit, composing it again writes the same notes, so a push
//! that finds `main` where it was composes nothing. It used to compose all of
//! `main` twice on every push (the checkpoint's sync, then the push) whenever
//! the project root had commits the remote had not merged.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn commit(dir: &Utf8Path, file: &str) {
    std::fs::write(dir.join(file), format!("{file}\n")).expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", file]);
}

/// Every compose writes one notes commit, labelled `compose`.
fn compositions(store: &Utf8Path) -> usize {
    git(store, &["log", "--format=%s", "refs/notes/newgit-composed"])
        .lines()
        .filter(|label| *label == "compose")
        .count()
}

fn push(workspace: &Utf8Path) {
    let pushed = git_output(workspace, &["push", "origin", "feature"]);
    assert!(
        pushed.status.success(),
        "push: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );
}

#[test]
fn a_push_composes_main_only_when_the_remote_root_moved() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    // A tracker in Git besides `root`, so `main` is a composition of two.
    std::fs::create_dir_all(fixture.store.join("private")).expect("mkdir");
    std::fs::write(fixture.store.join("private/x.txt"), "x\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "private"]);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "private", "--git", "--path", "private"],
    );
    fixture.ok(&fixture.store, &["merge", "main"]);

    // The project root is ahead of the remote's `main`, and `feature` is
    // spawned from it.
    commit(&fixture.store, "local.txt");
    fixture.ok(&fixture.store, &["spawn", "feature"]);
    let workspace = workspace_of(&fixture, "feature");

    commit(&workspace, "first.txt");
    push(&workspace);
    let composed = compositions(&fixture.store);

    for file in ["second.txt", "third.txt"] {
        commit(&workspace, file);
        push(&workspace);
        assert_eq!(
            compositions(&fixture.store),
            composed,
            "a push recomposed `main`, which has not moved"
        );
    }

    // `main` moves on the remote: the next push composes it again.
    commit(&fixture.store, "landed.txt");
    fixture.ok(&fixture.store, &["merge", "main"]);
    git(
        &workspace,
        &["pull", "-q", "--no-rebase", "--no-edit", "origin", "main"],
    );
    let before = compositions(&fixture.store);
    commit(&workspace, "fourth.txt");
    push(&workspace);
    assert!(
        compositions(&fixture.store) > before,
        "`main` moved, and the push did not compose it"
    );
}
