//! One commit, one commit on a mirror, whichever export makes it first.
//!
//! A commit merged on a mirror lands on the remote as a root commit naming the
//! mirror's own (`landed_view_not_split_again.rs`). A copy that starts an
//! instance on the remote's `main` as it *fetched* it, never pulled, used to
//! split that commit again, into a root commit of its own that had lost the
//! original. The instance's branch then reached the mirror with a second copy
//! of what the mirror's `main` already held, under another id, and every
//! commit above it with it: a pull request showing commits that are not its
//! own, by an author nobody is.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// Run git in a project root the way someone with newgit installed does: the
/// credential helper finds `newgit` by name, and their login under `HOME`.
fn git_as_user(fixture: &Fixture, dir: &Utf8Path, args: &[&str]) -> String {
    let binary = Utf8PathBuf::from(env!("CARGO_BIN_EXE_newgit"));
    let path = format!(
        "{}:{}",
        binary.parent().expect("a directory"),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("HOME", fixture.temp.join("home").as_str())
        .env("PATH", path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

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
    // Carving wrote `export = "inherit"`. Withheld, as t3code is from
    // Spheroman/newgit: the mirror's commits are then not the project's.
    set_export(&fixture.store, "private", None);
    let github = fixture.temp.join("github.git");
    git(
        &fixture.temp,
        &["init", "-q", "--bare", "-b", "main", github.as_str()],
    );
    set_export(
        &fixture.store,
        "root",
        Some(&export_to(&github, "github", "branches = true")),
    );
    set_export(&fixture.store, "access", Some("\"inherit\""));
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "export target"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["export", "--remote", "github"]);
    github
}

/// A pull request merged on `github`, signed as GitHub signs what it merges,
/// and landed on the remote by the first copy. Returns the mirror's commit.
fn merged_on_the_mirror(fixture: &Fixture, github: &Utf8Path) -> String {
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
    fixture.ok(&fixture.store, &["pull", "--remote", "github"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    id
}

/// Another copy of the project, which lands nothing the mirror merged, and
/// has composed the remote's `main` once already, as any copy in use has: an
/// instance spawned and its `origin` fetched.
fn another_copy(fixture: &Fixture) -> Utf8PathBuf {
    let elsewhere = fixture.temp.join("elsewhere");
    let cloned = fixture.newgit(
        &fixture.temp,
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
    git_as_user(fixture, &elsewhere, &["fetch", "-q", "origin"]);
    fixture.ok(&elsewhere, &["spawn", "early"]);
    git(
        &elsewhere,
        &["rev-parse", "--verify", "refs/newgit/remote-root/main"],
    );
    elsewhere
}

/// `feature`, spawned in `copy` on the remote's `main` as `copy` fetched it:
/// what a Claude Code worktree starts from when the project root is only
/// behind (`fresh_base`). Returns its workspace.
fn spawned_on_fetched_main(fixture: &Fixture, copy: &Utf8Path) -> Utf8PathBuf {
    git_as_user(fixture, copy, &["fetch", "-q", "origin"]);
    let base = git(copy, &["rev-parse", "refs/remotes/origin/main"]);
    fixture.ok(copy, &["spawn", "feature", "--from", &base]);
    Utf8PathBuf::from(fixture.ok(copy, &["status", "feature", "--path"]).trim())
}

fn commit_and_push(workspace: &Utf8Path, file: &str) -> String {
    std::fs::write(workspace.join(file), "mine\n").expect("write");
    git(workspace, &["add", "-A"]);
    git(workspace, &["commit", "-q", "-m", &format!("Add {file}")]);
    let pushed = git_output(workspace, &["push", "origin", "feature"]);
    assert!(
        pushed.status.success(),
        "push: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    )
}

/// The subjects of what `feature` has on the mirror that its `main` does not,
/// oldest first.
fn own_commits(github: &Utf8Path) -> Vec<String> {
    git(
        github,
        &["log", "--reverse", "--format=%s", "main..feature"],
    )
    .lines()
    .map(ToOwned::to_owned)
    .collect()
}

fn descends(github: &Utf8Path, ancestor: &str, descendant: &str) -> bool {
    git_output(
        github,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .status
    .success()
}

#[test]
fn a_branch_exported_before_main_is_the_mirrors_main_plus_its_own_commits() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let elsewhere = another_copy(&fixture);
    let theirs = merged_on_the_mirror(&fixture, &github);

    let workspace = spawned_on_fetched_main(&fixture, &elsewhere);
    commit_and_push(&workspace, "mine.txt");
    assert_eq!(
        git(&github, &["rev-parse", "feature^"]),
        theirs,
        "the branch is built on the mirror's own commit:\n{}",
        git(&github, &["log", "--format=%h %p %s", "-4", "feature"])
    );
    assert_eq!(own_commits(&github), ["Add mine.txt"]);

    // Then this copy's `main`: the same commit, not another one.
    git_as_user(
        &fixture,
        &elsewhere,
        &["pull", "-q", "--ff-only", "origin", "main"],
    );
    fixture.ok(&elsewhere, &["export", "--remote", "github"]);
    assert_eq!(git(&github, &["rev-parse", "main"]), theirs);
    assert_eq!(own_commits(&github), ["Add mine.txt"]);
}

#[test]
fn a_branch_exported_after_main_adds_only_its_own_commits() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let elsewhere = another_copy(&fixture);
    let theirs = merged_on_the_mirror(&fixture, &github);

    git_as_user(
        &fixture,
        &elsewhere,
        &["pull", "-q", "--ff-only", "origin", "main"],
    );
    fixture.ok(&elsewhere, &["export", "--remote", "github"]);
    assert_eq!(git(&github, &["rev-parse", "main"]), theirs);

    let workspace = spawned_on_fetched_main(&fixture, &elsewhere);
    commit_and_push(&workspace, "mine.txt");
    assert_eq!(git(&github, &["rev-parse", "feature^"]), theirs);
    assert_eq!(own_commits(&github), ["Add mine.txt"]);
}

/// The state a copy from before this was fixed is in: `feature`'s own commit
/// split onto a root commit of this copy's making, and the mirror holding the
/// branch built on a second copy of what its `main` has. Nothing told that
/// copy the base was the remote's, which is what forgetting where the remote's
/// `main` is does here.
fn exported_on_a_copy_of_main(
    fixture: &Fixture,
    github: &Utf8Path,
) -> (Utf8PathBuf, Utf8PathBuf, String) {
    let elsewhere = another_copy(fixture);
    let theirs = merged_on_the_mirror(fixture, github);
    let workspace = spawned_on_fetched_main(fixture, &elsewhere);
    git(
        &elsewhere,
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    commit_and_push(&workspace, "first.txt");
    assert!(
        !descends(github, &theirs, "feature"),
        "the broken state this test starts from: a copy of the mirror's commit"
    );
    git_as_user(fixture, &elsewhere, &["fetch", "-q", "origin"]);
    (elsewhere, workspace, theirs)
}

#[test]
fn a_branch_built_on_a_copy_is_rebuilt_on_the_mirrors_own_at_its_next_push() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (_, workspace, theirs) = exported_on_a_copy_of_main(&fixture, &github);
    let before = git(&workspace, &["rev-parse", "HEAD"]);

    let said = commit_and_push(&workspace, "second.txt");
    assert_eq!(
        git(&github, &["rev-parse", "feature~2"]),
        theirs,
        "{}",
        git(&github, &["log", "--format=%h %p %s", "-5", "feature"])
    );
    assert_eq!(own_commits(&github), ["Add first.txt", "Add second.txt"]);
    assert!(said.contains("replacing"), "the push says so: {said}");
    // The workspace's own commits are what they were: only the mirror's
    // differ.
    assert_eq!(git(&workspace, &["rev-parse", "HEAD^"]), before);
    assert_eq!(
        git(&workspace, &["rev-parse", "refs/remotes/github/feature"]),
        git(&github, &["rev-parse", "feature"])
    );
}

#[test]
fn exporting_the_instance_again_rebuilds_it_without_a_new_commit() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (elsewhere, workspace, theirs) = exported_on_a_copy_of_main(&fixture, &github);

    let stdout = fixture.ok(&elsewhere, &["export", "feature", "--remote", "github"]);
    assert!(stdout.contains("replacing"), "{stdout}");
    assert_eq!(git(&github, &["rev-parse", "feature^"]), theirs);
    assert_eq!(own_commits(&github), ["Add first.txt"]);
    assert_eq!(
        git(&workspace, &["rev-parse", "refs/remotes/github/feature"]),
        git(&github, &["rev-parse", "feature"])
    );

    // Nothing to do the second time.
    let again = fixture.ok(&elsewhere, &["export", "feature", "--remote", "github"]);
    assert!(!again.contains("replacing"), "{again}");
    assert_eq!(own_commits(&github), ["Add first.txt"]);
}

/// The mirror's branch is replaced only while it is what newgit last put
/// there: a commit someone pushed to it straight is never thrown away.
#[test]
fn a_branch_someone_pushed_to_on_the_mirror_is_left_as_it_is() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let (elsewhere, _, _) = exported_on_a_copy_of_main(&fixture, &github);

    let other = fixture.temp.join("someone");
    git(
        &fixture.temp,
        &[
            "clone",
            "-q",
            "-b",
            "feature",
            github.as_str(),
            other.as_str(),
        ],
    );
    std::fs::write(other.join("theirs.txt"), "theirs\n").expect("write");
    git(&other, &["add", "-A"]);
    git(
        &other,
        &["commit", "-q", "-m", "Pushed straight to the mirror"],
    );
    git(&other, &["push", "-q", "origin", "feature"]);
    let held = git(&github, &["rev-parse", "feature"]);

    let output = fixture.newgit(&elsewhere, &["export", "feature", "--remote", "github"]);
    assert!(!output.status.success(), "it should have been refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("left as it is"), "{stderr}");
    assert_eq!(git(&github, &["rev-parse", "feature"]), held);
}
