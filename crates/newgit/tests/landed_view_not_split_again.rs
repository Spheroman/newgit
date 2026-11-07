//! A commit merged on a mirror lands on the remote as a view commit, rebuilt
//! without GitHub's signature, and a root commit naming the mirror's original.
//! A copy of the project that did not land it sees only the rebuilt view. If
//! it splits that again, it makes a root commit of its own that has lost the
//! original, so every branch exported from it has ids the mirror has never
//! seen, and GitHub shows the pull request as conflicting.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// A project with a tracker the mirror does not ship, exported to a bare
/// repository standing in for GitHub.
fn mirrored(fixture: &Fixture) -> Utf8PathBuf {
    std::fs::create_dir_all(fixture.store.join("private")).expect("mkdir");
    std::fs::write(fixture.store.join("private/x.txt"), "x\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "private"]);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "private", "--git", "--path", "private"],
    );
    let github = fixture.temp.join("github.git");
    git(
        &fixture.temp,
        &["init", "-q", "--bare", "-b", "main", github.as_str()],
    );
    let dir = fixture.store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("github.toml"),
        format!("url = \"{github}\"\nbranches = true\ntrackers = [\"root\", \"access\"]\n"),
    )
    .expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "export target"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["export", "--remote", "github"]);
    github
}

/// A pull request merged on `github`, signed as GitHub signs what it merges.
fn signed_merge(fixture: &Fixture, github: &Utf8Path) -> String {
    let work = fixture.temp.join("pr");
    git(
        &fixture.temp,
        &["clone", "-q", github.as_str(), work.as_str()],
    );
    std::fs::write(work.join("pr.txt"), "pr\n").expect("write");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "A merged pull request"]);
    let raw = git(&work, &["cat-file", "commit", "HEAD"]);
    let (head, body) = raw.split_once("\n\n").expect("a commit");
    let object = format!(
        "{head}\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\n{body}\n"
    );
    let path = fixture.temp.join("signed-object");
    std::fs::write(&path, object).expect("write");
    let id = git(&work, &["hash-object", "-t", "commit", "-w", path.as_str()]);
    git(&work, &["update-ref", "refs/heads/main", &id]);
    git(&work, &["push", "-q", "origin", "main"]);
    id
}

/// The copy that does not land what the mirror merged, with `feature` spawned,
/// and that merge landed by the other copy and pulled into `feature`.
fn pulled_elsewhere(
    fixture: &Fixture,
    temp: &Utf8Path,
    github: &Utf8Path,
) -> (Utf8PathBuf, Utf8PathBuf, String) {
    let elsewhere = temp.join("elsewhere");
    let cloned = fixture.newgit(
        temp,
        &[
            "clone",
            "jack/store",
            elsewhere.as_str(),
            "--url",
            &fixture.remote.url,
        ],
    );
    assert!(
        cloned.status.success(),
        "clone: {}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    fixture.ok(&elsewhere, &["spawn", "feature"]);
    let record =
        std::fs::read_to_string(elsewhere.join(".newgit/branches/feature.toml")).expect("record");
    let line = record
        .lines()
        .find(|line| line.starts_with("workspace_path"))
        .expect("a workspace path")
        .to_owned();
    let workspace = Utf8PathBuf::from(line.split('"').nth(1).expect("quoted"));

    // Merged on GitHub, and landed by the first copy.
    let theirs = signed_merge(fixture, github);
    fixture.ok(&fixture.store, &["pull", "--remote", "github"]);
    fixture.ok(&fixture.store, &["merge", "main"]);

    git(
        &workspace,
        &["pull", "-q", "--no-rebase", "--no-edit", "origin", "main"],
    );
    (elsewhere, workspace, theirs)
}

fn pushes_descending_from(workspace: &Utf8Path, github: &Utf8Path, theirs: &str) {
    std::fs::write(workspace.join("mine.txt"), "mine\n").expect("write");
    git(workspace, &["add", "-A"]);
    git(workspace, &["commit", "-q", "-m", "Work after the merge"]);
    let pushed = git_output(workspace, &["push", "origin", "feature"]);
    assert!(
        pushed.status.success(),
        "push: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let exported = git(github, &["rev-parse", "feature"]);
    let descends = git_output(github, &["merge-base", "--is-ancestor", theirs, &exported]);
    assert!(
        descends.status.success(),
        "the exported branch lost the mirror's commit:\n{}",
        git(github, &["log", "--format=%h %p %s", "-5", &exported])
    );
}

#[test]
fn a_branch_from_another_copy_still_descends_from_what_the_mirror_merged() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (_, workspace, theirs) = pulled_elsewhere(&fixture, &temp, &github);
    pushes_descending_from(&workspace, &github, &theirs);
}

/// A copy that split the view before this was fixed holds a note naming a root
/// commit of its own. It has to heal by itself: nobody knows to clear it.
#[test]
fn a_note_naming_a_root_commit_the_remote_never_made_is_replaced() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (elsewhere, workspace, theirs) = pulled_elsewhere(&fixture, &temp, &github);

    // The note a split of its own left behind, on a commit in its store.
    let view = git(&workspace, &["rev-parse", "refs/remotes/origin/main"]);
    git(
        &workspace,
        &[
            "push",
            "-q",
            elsewhere.as_str(),
            &format!("{view}:refs/heads/seen"),
        ],
    );
    let tree = git(&elsewhere, &["rev-parse", &format!("{view}^{{tree}}")]);
    let own = git(
        &elsewhere,
        &["commit-tree", &tree, "-m", "a root commit of this copy's"],
    );
    git(
        &elsewhere,
        &[
            "notes",
            "--ref",
            "newgit-composed",
            "add",
            "-f",
            "-m",
            &format!("root {own}"),
            &view,
        ],
    );
    pushes_descending_from(&workspace, &github, &theirs);
}
