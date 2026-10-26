//! A `branches = true` export target on a project linked to a newgit remote:
//! a workspace's `origin` is the newgit endpoint, which `gh` cannot map to a
//! GitHub repository, so the target is a remote in the workspace that `gh`
//! resolves, and each push from the workspace exports the instance's branch
//! to it. `gh pr create` (non-interactive) then finds the branch exactly when
//! `refs/remotes/<target>/<branch>` is `HEAD` — so newgit records there the
//! commit the target really holds, and says `--head` when it is not `HEAD`.
//!
//! The newgit remote is the fake from `support/fake_remote.rs`; a bare
//! repository stands in for GitHub.

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn mirror(fixture: &Fixture, name: &str, trackers: &[&str]) -> Utf8PathBuf {
    let repo = fixture.temp.join(format!("{name}.git"));
    git(
        &fixture.temp,
        &["init", "-q", "--bare", "-b", "main", repo.as_str()],
    );
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
    repo
}

fn commit_work(workspace: &Utf8Path) -> String {
    std::fs::write(workspace.join("change.txt"), "agent work\n").expect("write");
    git(workspace, &["add", "change.txt"]);
    git(workspace, &["commit", "-q", "-m", "agent work"]);
    git(workspace, &["rev-parse", "HEAD"])
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

#[test]
fn the_workspace_gets_the_target_as_the_remote_gh_resolves() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirror(&fixture, "github", &["root", "project", "access"]);
    fixture.ok(&fixture.store, &["export", "--remote", "github"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");

    assert_eq!(
        git(&workspace, &["config", "remote.github.url"]),
        github.as_str()
    );
    assert_eq!(
        git(&workspace, &["config", "remote.github.gh-resolved"]),
        "base"
    );
    // What `gh pr create --fill` lists commits against.
    assert_eq!(
        git(&workspace, &["rev-parse", "refs/remotes/github/main"]),
        git(&github, &["rev-parse", "main"])
    );
}

#[test]
fn a_push_exports_the_branch_as_the_agents_own_commits_when_every_tracker_ships() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirror(&fixture, "github", &["root", "project", "access"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");

    let head = commit_work(&workspace);
    let stderr = pushed(&workspace);
    assert!(
        stderr.contains("`gh pr create` opens a pull request there"),
        "{stderr}"
    );
    assert_eq!(git(&github, &["rev-parse", "feature-a"]), head);
    // `gh` compares exactly this with `HEAD`.
    assert_eq!(
        git(&workspace, &["rev-parse", "refs/remotes/github/feature-a"]),
        head
    );
}

#[test]
fn a_filtered_target_gets_other_commits_and_the_push_says_head() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let public = mirror(&fixture, "public", &["root"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");

    let head = commit_work(&workspace);
    let stderr = pushed(&workspace);
    assert!(stderr.contains("gh pr create --head feature-a"), "{stderr}");
    let exported = git(&public, &["rev-parse", "feature-a"]);
    assert_ne!(
        exported, head,
        "`.newgit` is withheld, so the commits differ"
    );
    assert!(files_at(&public, "feature-a").contains("change.txt"));
    assert!(!files_at(&public, "feature-a").contains(".newgit"));
    // The tracking ref is what the target holds, never a claim it is `HEAD`.
    assert_eq!(
        git(&workspace, &["rev-parse", "refs/remotes/public/feature-a"]),
        exported
    );
}

#[test]
fn pushing_to_the_target_directly_is_refused_naming_origin() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirror(&fixture, "github", &["root"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    commit_work(&workspace);

    let output = push(&workspace, "github");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("git push origin feature-a"), "{stderr}");
    assert_eq!(git(&github, &["for-each-ref"]), "", "nothing reached it");
}

#[test]
fn a_target_without_branches_gets_no_remote_and_no_branch() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirror(&fixture, "github", &["root"]);
    let path = fixture.store.join(".newgit/exports/github.toml");
    let text = std::fs::read_to_string(&path).expect("read");
    std::fs::write(&path, text.replace("branches = true\n", "")).expect("write");
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    commit_work(&workspace);

    let stderr = pushed(&workspace);
    assert!(!stderr.contains("exported"), "{stderr}");
    assert!(
        git_output(&workspace, &["config", "remote.github.url"])
            .status
            .code()
            == Some(1)
    );
    assert_eq!(git(&github, &["for-each-ref"]), "");
}
