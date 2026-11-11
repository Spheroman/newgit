//! A workspace gets each `branches = true` target's exported branch, so `gh pr
//! create --fill` can list a pull request's commits. That branch can be a
//! whole tracker's history under ids of its own, and fetched into each new
//! workspace it was 17s of a 21s t3code spawn. The source template carries
//! it instead: fetched there once, it reaches each workspace in the same
//! `clonefile` as the rest, and the workspace's own fetch finds every object
//! already present.
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
    set_export(
        &fixture.store,
        "app",
        Some(&export_to(&github, "app", "branches = true")),
    );
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "export app"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["export", "--remote", "app"]);
}

fn refs_under(repo: &Utf8Path, prefix: &str) -> Vec<String> {
    let output = git_output(repo, &["for-each-ref", "--format=%(refname)", prefix]);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

fn rev(repo: &Utf8Path, name: &str) -> String {
    let output = git_output(repo, &["rev-parse", "--verify", name]);
    assert!(output.status.success(), "{name} resolves in {repo}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn the_template_carries_the_exported_branch_and_the_workspace_keeps_only_its_own_ref() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    rooted_target(&fixture);

    fixture.ok(&fixture.store, &["spawn", "with-app"]);
    let workspace = workspace_of(&fixture, "with-app");
    assert_eq!(
        rev(&workspace, "refs/remotes/app/main"),
        rev(&fixture.store, "refs/newgit/exports/app/main"),
        "gh gets the target's branch as the store exported it"
    );
    assert!(
        refs_under(&workspace, "refs/newgit-template").is_empty(),
        "the template's carried refs are not the workspace's"
    );

    if cfg!(target_os = "macos") {
        let template = workspace
            .parent()
            .expect("workspace root")
            .join(".source-template");
        assert_eq!(
            refs_under(&template, "refs/newgit-template"),
            vec!["refs/newgit-template/refs/newgit/exports/app/main".to_owned()],
            "the template fetched the branch, so the workspace's fetch had nothing to move"
        );
    }
}
