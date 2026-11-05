//! `newgit pull --remote` and a rooted export on a project linked to a newgit
//! remote (#142 review): until `newgit merge main` publishes a pulled-back
//! `main`, a repeat pull finds nothing and the export says to merge; and a
//! remote that fails never lets either decide on refs that may be stale.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn fails(fixture: &Fixture, args: &[&str]) -> String {
    let output = fixture.newgit(&fixture.store, args);
    assert!(
        !output.status.success(),
        "newgit {args:?} should have failed:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// t3code imported and published, exported rooted to `t3code.git`, a copy
/// of its upstream. Returns that copy.
fn exported(fixture: &Fixture) -> Utf8PathBuf {
    let upstream = fixture.temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "newgit"]);
    std::fs::write(upstream.join("app.ts"), "app\n").expect("write");
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "t3: first"]);
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
    let dir = fixture.store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("t3code.toml"),
        format!("url = \"{github}\"\nbranch = \"newgit\"\nroot = \"t3code\"\n"),
    )
    .expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "export target"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["export", "--remote", "t3code"]);
    github
}

/// A pull request merged on `github`'s `newgit`. Returns the new tip.
fn merge_pr(fixture: &Fixture, github: &Utf8Path) -> String {
    let clone = fixture.temp.join("pr");
    git(
        &fixture.temp,
        &[
            "clone",
            "-q",
            "-b",
            "newgit",
            github.as_str(),
            clone.as_str(),
        ],
    );
    std::fs::write(clone.join("pr.ts"), "pr\n").expect("write");
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "t3: from a PR"]);
    git(&clone, &["push", "-q", "origin", "newgit"]);
    git(&clone, &["rev-parse", "HEAD"])
}

#[test]
fn until_merge_main_publishes_it_a_pull_again_finds_nothing_and_export_says_to_merge() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = exported(&fixture);
    let theirs = merge_pr(&fixture, &github);

    let stdout = fixture.ok(&fixture.store, &["pull", "--remote", "t3code"]);
    assert!(stdout.contains("Pulled 1 commit"), "{stdout}");
    let stdout = fixture.ok(&fixture.store, &["pull", "--remote", "t3code"]);
    assert!(stdout.contains("Nothing new"), "{stdout}");

    let stderr = fails(&fixture, &["export", "--remote", "t3code"]);
    assert!(
        stderr.contains("`newgit merge main`, then export again"),
        "{stderr}"
    );
    assert_eq!(git(&github, &["rev-parse", "newgit"]), theirs);

    fixture.ok(&fixture.store, &["merge", "main"]);
    assert_eq!(
        git(
            &fixture.remote.repo("jack.store.t.t3code"),
            &["rev-parse", "main"]
        ),
        theirs,
        "published"
    );
    fixture.ok(&fixture.store, &["export", "--remote", "t3code"]);
    assert_eq!(git(&github, &["rev-parse", "newgit"]), theirs, "a no-op");
}

#[test]
fn a_remote_that_fails_its_credentials_fails_every_command_that_reads_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = exported(&fixture);
    merge_pr(&fixture, &github);
    let head = git(&fixture.store, &["rev-parse", "HEAD"]);
    fixture.remote.state.lock().expect("state").fail_credentials = true;

    for args in [
        &["pull", "t3code"][..],
        &["export", "--remote", "t3code"][..],
        &["pull", "--remote", "t3code"][..],
    ] {
        let stderr = fails(&fixture, args);
        assert!(stderr.contains("503"), "{args:?}: {stderr}");
    }
    assert_eq!(
        git(&fixture.store, &["rev-parse", "HEAD"]),
        head,
        "nothing moved"
    );
}

#[test]
fn a_tracker_repo_that_cannot_be_fetched_fails_every_command_that_reads_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = exported(&fixture);
    merge_pr(&fixture, &github);
    let head = git(&fixture.store, &["rev-parse", "HEAD"]);
    // The repo is there to hand out a credential for, but Git cannot read
    // it: a fetch that fails for any reason but a missing branch.
    let repo = fixture.remote.repo("jack.store.t.t3code");
    std::fs::remove_dir_all(repo.join("objects")).expect("break it");

    for args in [
        &["pull", "t3code"][..],
        &["export", "--remote", "t3code"][..],
        &["pull", "--remote", "t3code"][..],
    ] {
        fails(&fixture, args);
    }
    assert_eq!(
        git(&fixture.store, &["rev-parse", "HEAD"]),
        head,
        "nothing moved"
    );
}
