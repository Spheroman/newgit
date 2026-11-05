//! `gh` in a workspace resolves to the `branches = true` export target the
//! instance's work belongs to (issue #143): the one that ships a tracker the
//! work touched. Work that belongs to two targets is exported to both, `gh`
//! resolves neither, and the push names the `gh pr create -R <repo> --head
//! <branch>` to run for each. A direct push to a target stays refused.
//!
//! The project has two trackers in Git: `root` (the project's code, shipped by
//! `code`) and `b` (`apps/b`, shipped by `apps-b`). A bare repository stands
//! in for each GitHub repo.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn bare(fixture: &Fixture, name: &str) -> Utf8PathBuf {
    let repo = fixture.temp.join(format!("{name}.git"));
    git(
        &fixture.temp,
        &["init", "-q", "--bare", "-b", "main", repo.as_str()],
    );
    repo
}

fn define(fixture: &Fixture, name: &str, repo: &Utf8Path, trackers: &[&str]) {
    let dir = fixture.store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let trackers: Vec<String> = trackers.iter().map(|t| format!("\"{t}\"")).collect();
    std::fs::write(
        dir.join(format!("{name}.toml")),
        format!(
            "url = \"{repo}\"\nbranches = true\ntrackers = [{}]\n",
            trackers.join(", ")
        ),
    )
    .expect("write");
}

/// A tracker in Git `b` owning `apps/b`: carving it is a commit of its own.
fn with_tracker_b(fixture: &Fixture) {
    std::fs::create_dir_all(fixture.store.join("apps/b")).expect("mkdir");
    std::fs::write(fixture.store.join("apps/b/index.js"), "b v1\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "apps/b"]);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "b", "--git", "--path", "apps/b"],
    );
}

/// Both targets defined and committed, `feature-a` spawned.
fn two_targets(fixture: &Fixture) -> (Utf8PathBuf, Utf8PathBuf, Utf8PathBuf) {
    with_tracker_b(fixture);
    let code = bare(fixture, "code");
    let apps = bare(fixture, "apps-b");
    define(fixture, "code", &code, &["root", "access"]);
    define(fixture, "apps-b", &apps, &["b"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "export targets"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(fixture, "feature-a");
    (code, apps, workspace)
}

fn commit(workspace: &Utf8Path, path: &str) {
    let file = workspace.join(path);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, "agent work\n").expect("write");
    git(workspace, &["add", path]);
    git(
        workspace,
        &["commit", "-q", "-m", &format!("work in {path}")],
    );
}

fn push(workspace: &Utf8Path, remote: &str) -> Output {
    git_output(workspace, &["push", remote, "feature-a"])
}

fn pushed(workspace: &Utf8Path) -> String {
    let output = push(workspace, "origin");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "push failed: {stderr}");
    stderr
}

/// `remote.<name>.gh-resolved` in the workspace, if set.
fn resolved(workspace: &Utf8Path, remote: &str) -> Option<String> {
    let output = git_output(
        workspace,
        &["config", &format!("remote.{remote}.gh-resolved")],
    );
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[test]
fn one_target_is_the_one_gh_resolves() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = bare(&fixture, "github");
    define(&fixture, "github", &github, &["root", "access"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    assert_eq!(resolved(&workspace, "github").as_deref(), Some("base"));

    commit(&workspace, "change.txt");
    let stderr = pushed(&workspace);
    assert!(
        stderr.contains("`gh pr create` opens a pull request there"),
        "{stderr}"
    );
    assert_eq!(resolved(&workspace, "github").as_deref(), Some("base"));
}

#[test]
fn work_in_one_targets_tracker_resolves_gh_to_that_target() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (code, apps, workspace) = two_targets(&fixture);
    // Two candidates and no work yet: `gh` resolves neither.
    assert_eq!(resolved(&workspace, "code"), None);
    assert_eq!(resolved(&workspace, "apps-b"), None);

    commit(&workspace, "apps/b/feature.js");
    let stderr = pushed(&workspace);
    assert_eq!(resolved(&workspace, "apps-b").as_deref(), Some("base"));
    assert_eq!(resolved(&workspace, "code"), None);
    // The branch goes to `apps-b` alone. A target that ships `b` without
    // `root` exports it as its own root, which is #141: until then the
    // export itself says why it could not, so only the routing is checked.
    assert!(stderr.contains("`apps-b`"), "{stderr}");
    let _ = &apps;
    assert_eq!(
        git(&code, &["for-each-ref"]),
        "",
        "the other target, whose trackers the work never touched, got nothing"
    );
    assert!(!stderr.contains("`code`"), "{stderr}");
}

#[test]
fn the_other_targets_work_resolves_the_other_way() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (code, apps, workspace) = two_targets(&fixture);

    commit(&workspace, "change.txt");
    pushed(&workspace);
    assert_eq!(resolved(&workspace, "code").as_deref(), Some("base"));
    assert_eq!(resolved(&workspace, "apps-b"), None);
    assert!(files_at(&code, "feature-a").contains("change.txt"));
    assert_eq!(git(&apps, &["for-each-ref"]), "");
}

#[test]
fn work_in_both_goes_to_both_and_gh_resolves_neither() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (code, apps, workspace) = two_targets(&fixture);

    commit(&workspace, "change.txt");
    commit(&workspace, "apps/b/feature.js");
    let stderr = pushed(&workspace);

    assert_eq!(resolved(&workspace, "code"), None);
    assert_eq!(resolved(&workspace, "apps-b"), None);
    assert!(files_at(&code, "feature-a").contains("change.txt"));
    let command = format!("gh pr create -R {code} --head feature-a");
    assert!(stderr.contains(&command), "no `{command}` in:\n{stderr}");
    // `apps-b` ships `b` without `root`, which exports as its own root
    // (#141); until then it is only routed to, and says why it could not.
    let _ = &apps;
    assert!(stderr.contains("`apps-b`"), "{stderr}");
}

#[test]
fn re_syncing_the_workspace_keeps_gh_where_the_push_left_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (_code, _apps, workspace) = two_targets(&fixture);

    commit(&workspace, "apps/b/feature.js");
    pushed(&workspace);
    assert_eq!(resolved(&workspace, "apps-b").as_deref(), Some("base"));

    // A checkpoint re-syncs the workspace's remotes, as every one does, and
    // reaches the same answer the push did.
    fixture.ok(&fixture.store, &["checkpoint", "feature-a"]);
    assert_eq!(resolved(&workspace, "apps-b").as_deref(), Some("base"));
    assert_eq!(resolved(&workspace, "code"), None);
}

#[test]
fn pushing_to_a_target_directly_is_still_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (code, apps, workspace) = two_targets(&fixture);
    commit(&workspace, "apps/b/feature.js");

    for remote in ["code", "apps-b"] {
        let output = push(&workspace, remote);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{remote}: {stderr}");
        assert!(stderr.contains("git push origin feature-a"), "{stderr}");
    }
    assert_eq!(git(&code, &["for-each-ref"]), "", "nothing reached it");
    assert_eq!(git(&apps, &["for-each-ref"]), "", "nothing reached it");
}
