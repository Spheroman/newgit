// Helpers for the tests of a mirror's commits and branches
// (`one_mirror_commit.rs`, `mirror_branch_lease.rs`, `remote_main_pins.rs`):
// a project exported to a bare repository standing in for GitHub, a pull
// request merged there, and another copy of the project that did not land it.
// `include!`d after `fake_remote.rs`.

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
