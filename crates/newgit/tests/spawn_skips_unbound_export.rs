//! A workspace gets each `branches = true` target's exported branch, so `gh pr
//! create --fill` can list a pull request's commits. For a target rooted at a
//! tracker the instance leaves out, that branch is the tracker's whole
//! history, under ids of its own: fetching t3code's was 6.6s of an 8.6s spawn
//! here, for a target no work of that instance can go to.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// A project with `app` in Git, exported on its own to a rooted target that
/// takes branches.
fn rooted_target(fixture: &Fixture) {
    std::fs::create_dir_all(fixture.store.join("app")).expect("mkdir");
    std::fs::write(fixture.store.join("app/main.ts"), "app\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "app"]);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "app", "--git", "--path", "app"],
    );
    let github = fixture.temp.join("app.git");
    git(
        &fixture.temp,
        &["init", "-q", "--bare", "-b", "main", github.as_str()],
    );
    let dir = fixture.store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("app.toml"),
        format!("url = \"{github}\"\nroot = \"app\"\nbranches = true\n"),
    )
    .expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "export app"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["export", "--remote", "app"]);
}

fn has_ref(workspace: &Utf8Path, name: &str) -> bool {
    git_output(workspace, &["rev-parse", "-q", "--verify", name])
        .status
        .success()
}

#[test]
fn a_workspace_without_the_root_of_a_target_does_not_fetch_its_branch() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    rooted_target(&fixture);

    fixture.ok(&fixture.store, &["spawn", "with-app"]);
    assert!(
        has_ref(&workspace_of(&fixture, "with-app"), "refs/remotes/app/main"),
        "bound: gh gets the target's branch"
    );

    fixture.ok(&fixture.store, &["spawn", "without-app", "--drop", "app"]);
    let workspace = workspace_of(&fixture, "without-app");
    assert!(
        !has_ref(&workspace, "refs/remotes/app/main"),
        "left out: nothing of its history is fetched"
    );

    // Bound later, a checkpoint routes again and fetches it then.
    fixture.ok(&workspace, &["tracker", "use", "app"]);
    std::fs::write(workspace.join("wip.txt"), "wip\n").expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    assert!(has_ref(&workspace, "refs/remotes/app/main"));
}
