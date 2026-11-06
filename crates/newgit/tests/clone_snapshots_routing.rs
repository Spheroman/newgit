//! What a snapshot clone must get right beyond fetching: where a branch is
//! exported, a remote that refuses pushes from a shallow repository, and a
//! checkpoint whose root reaches a snapshot.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

const HISTORY_LINE: &str = "'s history from";

/// t3code mounted at `apps/t3code`, three commits long. With `branches`, a
/// rooted target for it that takes instance branches, and a bare repository
/// behind it, which is returned.
fn published(fixture: &Fixture, branches: bool) -> Utf8PathBuf {
    let upstream = fixture.temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "main"]);
    for n in 1..=3 {
        std::fs::write(upstream.join("app.ts"), format!("app {n}\n")).expect("write");
        git(&upstream, &["add", "-A"]);
        git(&upstream, &["commit", "-q", "-m", &format!("t3: {n}")]);
    }
    let github = fixture.temp.join("t3code.git");
    git(
        &fixture.temp,
        &["clone", "-q", "--bare", upstream.as_str(), github.as_str()],
    );
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "t3code",
            "--git",
            "--mount",
            "apps/t3code",
            "--from",
            upstream.as_str(),
        ],
    );
    if branches {
        let dir = fixture.store.join(".newgit/exports");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("t3code.toml"),
            format!("url = \"{github}\"\nbranch = \"main\"\nroot = \"t3code\"\nbranches = true\n"),
        )
        .expect("write");
    }
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "mounted"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    github
}

fn clone(fixture: &Fixture, name: &str) -> Utf8PathBuf {
    let dir = fixture.temp.join(name);
    fixture.ok(
        &fixture.temp,
        &[
            "clone",
            "jack/store",
            dir.as_str(),
            "--url",
            &fixture.remote.url,
        ],
    );
    dir
}

fn run(fixture: &Fixture, dir: &Utf8Path, args: &[&str]) -> (String, String) {
    let output = fixture.newgit(dir, args);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    );
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{stdout}\n{stderr}"
    );
    (stdout, stderr)
}

fn spawn(fixture: &Fixture, clone: &Utf8Path, name: &str) -> Utf8PathBuf {
    run(fixture, clone, &["spawn", name]);
    Utf8PathBuf::from(run(fixture, clone, &["status", name, "--path"]).0.trim())
}

/// `git push` as the logged-in user: the hook it runs needs their login.
fn push(fixture: &Fixture, workspace: &Utf8Path) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(workspace.as_str())
        .args(["push", "-q", "origin", "HEAD"])
        .env("HOME", fixture.temp.join("home").as_str())
        .output()
        .expect("git runs")
}

fn commit(workspace: &Utf8Path, path: &str, message: &str) {
    std::fs::write(workspace.join(path), "work\n").expect("write");
    git(workspace, &["add", "-A"]);
    git(workspace, &["commit", "-q", "-m", message]);
}

/// Make every push newgit runs from `clone` fail as a remote that will not
/// take `shallow` lines would, for as long as `clone` is shallow. A hook,
/// reached through the fixture's home, because real Git takes those lines.
fn refuse_shallow_pushes(fixture: &Fixture, clone: &Utf8Path) {
    let hooks = fixture.temp.join("refusing-hooks");
    std::fs::create_dir_all(&hooks).expect("mkdir");
    let hook = hooks.join("pre-receive");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nif test -s '{clone}/.git/shallow'; then\n  echo \"protocol error: \
             expected old/new/ref, got 'shallow'\" >&2\n  exit 1\nfi\n"
        ),
    )
    .expect("write");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::write(
        fixture.temp.join("home/.gitconfig"),
        format!("[core]\n\thooksPath = {hooks}\n"),
    )
    .expect("write");
}

#[test]
fn a_remote_refusing_shallow_pushes_gets_every_history_then_the_push() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture, false);
    let snapshot = clone(&fixture, "snapshot");
    let workspace = spawn(&fixture, &snapshot, "work");
    commit(&workspace, "apps/t3code/mine.ts", "work in t3code");
    assert_eq!(
        git(&snapshot, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
    refuse_shallow_pushes(&fixture, &snapshot);

    let (out, err) = run(&fixture, &workspace, &["checkpoint"]);
    assert!(err.contains("refused a push"), "{out}\n{err}");
    assert!(err.contains(HISTORY_LINE), "{err}");
    assert_eq!(
        git(&snapshot, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
    let (_, merged) = run(&fixture, &snapshot, &["merge", "work", "--into", "main"]);
    assert!(!merged.contains("refused a push"), "{merged}");
    assert!(files_at(&fixture.remote.repo("jack.store.t.t3code"), "main").contains("mine.ts"));
}

#[test]
fn a_push_refused_for_another_reason_fetches_no_history() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture, false);
    let snapshot = clone(&fixture, "snapshot");
    let workspace = spawn(&fixture, &snapshot, "work");
    commit(&workspace, "apps/t3code/mine.ts", "work in t3code");

    let hooks = fixture.temp.join("hooks");
    std::fs::create_dir_all(&hooks).expect("mkdir");
    let hook = hooks.join("pre-receive");
    std::fs::write(&hook, "#!/bin/sh\necho 'quota exceeded' >&2\nexit 1\n").expect("write");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::write(
        fixture.temp.join("home/.gitconfig"),
        format!("[core]\n\thooksPath = {hooks}\n"),
    )
    .expect("write");

    let output = fixture.newgit(&workspace, &["checkpoint"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains(HISTORY_LINE), "{stderr}");
    assert_eq!(
        git(&snapshot, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
}

#[test]
fn work_in_root_does_not_fetch_a_rooted_targets_history_on_push() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = published(&fixture, true);
    let snapshot = clone(&fixture, "snapshot");
    let workspace = spawn(&fixture, &snapshot, "root-only");
    commit(&workspace, "root.txt", "work in root");

    let pushed = push(&fixture, &workspace);
    let stderr = String::from_utf8_lossy(&pushed.stderr);
    assert!(pushed.status.success(), "{stderr}");
    assert!(!stderr.contains(HISTORY_LINE), "{stderr}");
    assert!(
        !git_output(
            &github,
            &["rev-parse", "-q", "--verify", "refs/heads/root-only"]
        )
        .status
        .success(),
        "nothing of t3code to export"
    );
    assert_eq!(
        git(&snapshot, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
}

#[test]
fn work_in_the_child_still_reaches_its_rooted_target() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = published(&fixture, true);
    let snapshot = clone(&fixture, "snapshot");
    let workspace = spawn(&fixture, &snapshot, "in-child");
    commit(&workspace, "apps/t3code/mine.ts", "work in t3code");

    let pushed = push(&fixture, &workspace);
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    assert!(files_at(&github, "refs/heads/in-child").contains("mine.ts"));
}

#[test]
fn branches_of_a_child_that_diverged_across_snapshots_are_told_apart_with_its_history() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = published(&fixture, true);
    let snapshot = clone(&fixture, "snapshot");
    let workspace = spawn(&fixture, &snapshot, "stale");

    // `main` here moves to a snapshot that says nothing about this instance's
    // pin: no ancestry links them on a cut history.
    let later = clone(&fixture, "later");
    let moved = spawn(&fixture, &later, "moved");
    commit(&moved, "apps/t3code/moved.ts", "moved t3code");
    run(&fixture, &later, &["merge", "moved", "--into", "main"]);
    let tip = git(
        &fixture.remote.repo("jack.store.t.t3code"),
        &["rev-parse", "main"],
    );
    let snapshot_held = |commit: &str| {
        git(
            &snapshot,
            &[
                "fetch",
                "-q",
                "--depth=1",
                fixture.remote.repo("jack.store.t.t3code").as_str(),
                &format!("+{commit}:refs/newgit/snapshots/t3code/{commit}"),
            ],
        );
    };
    snapshot_held(&tip);
    git(
        &snapshot,
        &["update-ref", "refs/newgit/trackers/t3code/main", &tip],
    );

    commit(&workspace, "root.txt", "work in root");
    let pushed = push(&fixture, &workspace);
    let stderr = String::from_utf8_lossy(&pushed.stderr);
    assert!(pushed.status.success(), "{stderr}");
    assert!(stderr.contains(HISTORY_LINE), "{stderr}");
    assert!(
        !git_output(
            &github,
            &["rev-parse", "-q", "--verify", "refs/heads/stale"]
        )
        .status
        .success(),
        "its pin is behind `main`, so it touched nothing there"
    );
}
