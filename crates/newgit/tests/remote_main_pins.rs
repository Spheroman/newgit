//! The remote's `main` counts as composed here only with every pin it has.
//!
//! Composing leaves out a pin that is not here. A copy that composed the
//! remote's `main` before one of its trackers' commits arrived noted view
//! commits the remote never served, and left the ones it did serve unnoted,
//! so an instance's split made root commits of its own for them: copies, on
//! a mirror, of commits its `main` has. That is how #459 and #460 reached
//! GitHub with a second copy of the #436 merge (`one_mirror_commit.rs`).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");
include!("support/mirror.rs");

/// `git push` from `workspace` as someone with newgit installed, so the
/// workspace can read the remote: what it said, stdout and stderr.
fn push_as_user(fixture: &Fixture, workspace: &Utf8Path, file: &str) -> String {
    std::fs::write(workspace.join(file), "mine\n").expect("write");
    git(workspace, &["add", "-A"]);
    git(workspace, &["commit", "-q", "-m", &format!("Add {file}")]);
    let binary = Utf8PathBuf::from(env!("CARGO_BIN_EXE_newgit"));
    let path = format!(
        "{}:{}",
        binary.parent().expect("a directory"),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace.as_str())
        .args(["push", "origin", "feature"])
        .env("HOME", fixture.temp.join("home").as_str())
        .env("PATH", path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "push: {said}");
    said
}

/// `private` moves on in the first copy after the second was made, and a
/// pull request merged on the mirror lands on top: the remote's `main` pins
/// a `private` commit the second copy has never had. Returns that commit and
/// the mirror's.
fn private_moved_then_merged(fixture: &Fixture, github: &Utf8Path) -> (String, String) {
    std::fs::write(fixture.store.join("private/x.txt"), "x two\n").expect("write");
    git(&fixture.store, &["commit", "-qam", "private, again"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    let pin = git(
        &fixture.store,
        &["rev-parse", "refs/newgit/trackers/private/main"],
    );
    let theirs = merged_on_the_mirror(fixture, github);
    (pin, theirs)
}

fn private_repo(fixture: &Fixture) -> Utf8PathBuf {
    fixture.remote.repo("jack.store.t.private")
}

/// The remote's `main` composed here while `private`'s commit was not, as a
/// newgit from before every pin was checked left it: recorded as composed,
/// its view noted with the remote's root commit, and `private` missing from
/// that view. The view commits the remote serves stay unnoted.
fn composed_without_the_pin(fixture: &Fixture, copy: &Utf8Path) {
    let remote_root = git(
        &fixture.remote.repo("jack.store.t.root"),
        &["rev-parse", "refs/heads/main"],
    );
    git(
        copy,
        &[
            "fetch",
            "-q",
            fixture.remote.repo("jack.store.t.root").as_str(),
            "+refs/heads/main:refs/newgit/remote-root/main",
        ],
    );
    let served = git(copy, &["rev-parse", "refs/remotes/origin/main^{tree}"]);
    let without: String = git(copy, &["ls-tree", &served])
        .lines()
        .filter(|line| !line.ends_with("\tprivate"))
        .map(|line| format!("{line}\n"))
        .collect();
    let listing = fixture.temp.join("without-private");
    std::fs::write(&listing, without).expect("write");
    let output = Command::new("git")
        .arg("-C")
        .arg(copy.as_str())
        .args(["mktree"])
        .stdin(std::fs::File::open(&listing).expect("open"))
        .output()
        .expect("git runs");
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let view = git(copy, &["commit-tree", &tree, "-m", "main, composed"]);
    git(
        copy,
        &["update-ref", "refs/newgit/views/branches/main", &view],
    );
    git(
        copy,
        &[
            "notes",
            "--ref",
            "refs/notes/newgit-composed",
            "add",
            "-f",
            "-m",
            &format!("root {remote_root}"),
            &view,
        ],
    );
}

/// The commit arrives later; the next split composes `main` again and
/// builds on the remote's root commit, not a copy of it.
#[test]
fn main_composed_without_a_pin_is_composed_again_once_it_arrives() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let elsewhere = another_copy(&fixture);
    let (pin, theirs) = private_moved_then_merged(&fixture, &github);
    git_as_user(&fixture, &elsewhere, &["fetch", "-q", "origin"]);
    composed_without_the_pin(&fixture, &elsewhere);

    // It arrives, as any fetch of `private` brings it.
    git(
        &elsewhere,
        &[
            "fetch",
            "-q",
            private_repo(&fixture).as_str(),
            "+refs/heads/main:refs/newgit/remote/private/main",
        ],
    );
    git(
        &elsewhere,
        &["cat-file", "-e", &format!("{pin}^{{commit}}")],
    );

    let workspace = spawned_on_fetched_main(&fixture, &elsewhere);
    let said = push_as_user(&fixture, &workspace, "mine.txt");
    assert_eq!(
        git(&github, &["rev-parse", "feature^"]),
        theirs,
        "built on a copy of the mirror's commit:\n{}\n{said}",
        git(&github, &["log", "--format=%h %p %s", "-4", "feature"])
    );
    assert_eq!(own_commits(&github), ["Add mine.txt"]);
}

/// While the remote's repo for a tracker does not send the commit its
/// `main` pins, a split says so, rather than compose `main` without it and
/// take that as done, and the next split tries again. Here the instance's
/// own split makes that very commit of the view (splitting is a function of
/// it), so the push's second split finds it, and the branch is the mirror's
/// `main` plus its own commits from the first push on.
#[test]
fn a_pin_the_remote_did_not_send_is_said_and_tried_again() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = mirrored(&fixture);
    let elsewhere = another_copy(&fixture);
    let (pin, theirs) = private_moved_then_merged(&fixture, &github);
    git_as_user(&fixture, &elsewhere, &["fetch", "-q", "origin"]);

    // The remote's `private` repo has lost the commit its `main` pins.
    let private = private_repo(&fixture);
    let saved = fixture.temp.join("private.bundle");
    git(
        &private,
        &["bundle", "create", "-q", saved.as_str(), "refs/heads/main"],
    );
    git(
        &private,
        &["update-ref", "refs/heads/main", &format!("{pin}^")],
    );
    git(&private, &["reflog", "expire", "--expire=now", "--all"]);
    git(&private, &["gc", "-q", "--prune=now"]);
    assert!(
        !git_output(&private, &["cat-file", "-e", &pin])
            .status
            .success()
    );

    let workspace = spawned_on_fetched_main(&fixture, &elsewhere);
    let said = push_as_user(&fixture, &workspace, "first.txt");
    assert!(
        said.contains(&format!("pins `private` at {}", &pin[..12]))
            && said.contains("which its repo did not send"),
        "{said}"
    );
    assert_eq!(
        git(&github, &["rev-parse", "feature^"]),
        theirs,
        "{}\n{said}",
        git(&github, &["log", "--format=%h %p %s", "-4", "feature"])
    );

    // Once the remote has it again, nothing more is said.
    git(
        &private,
        &[
            "fetch",
            "-q",
            saved.as_str(),
            "+refs/heads/main:refs/heads/main",
        ],
    );
    let said = push_as_user(&fixture, &workspace, "second.txt");
    assert!(!said.contains("did not send"), "{said}");
    assert_eq!(git(&github, &["rev-parse", "feature~2"]), theirs);
    assert_eq!(own_commits(&github), ["Add first.txt", "Add second.txt"]);
}
