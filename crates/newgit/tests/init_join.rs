//! `newgit init` in a clone of a project's GitHub mirror, whose committed
//! config names the project on a remote. A member joins it in place and ends
//! where `newgit clone` ends; anyone else gets a local adoption that cannot
//! pass for linked (docs/design/remote.md, *Joining from a GitHub clone*).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// t3code mounted at `apps/t3code`, `[remote]` committed, and every commit
/// exported to a mirror shipping only `root`, as Spheroman/newgit's does, so
/// its ids differ from the view's. Returns the mirror.
fn mirrored(fixture: &Fixture) -> Utf8PathBuf {
    let upstream = fixture.temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.join("app.ts"), "app\n").expect("write");
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "t3: first"]);
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
    let mirror = fixture.temp.join("mirror.git");
    git(
        &fixture.temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    let dir = fixture.store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("github.toml"),
        format!("url = \"{mirror}\"\ntrackers = [\"root\"]\n"),
    )
    .expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "export target"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["export", "--remote", "github"]);
    mirror
}

fn github_clone(fixture: &Fixture, mirror: &Utf8Path, name: &str) -> Utf8PathBuf {
    let dir = fixture.temp.join(name);
    git(
        &fixture.temp,
        &["clone", "-q", mirror.as_str(), dir.as_str()],
    );
    dir
}

/// Everything a refused `init` must leave as it was.
fn state(dir: &Utf8Path) -> String {
    [
        git(dir, &["rev-parse", "HEAD"]),
        git(dir, &["status", "--porcelain"]),
        git(dir, &["for-each-ref"]),
        git(dir, &["config", "--get-regexp", "^remote\\."]),
        git(dir, &["rev-parse", "--is-shallow-repository"]),
        dir.join(".newgit/local").exists().to_string(),
        dir.join(".git/newgit-join").exists().to_string(),
    ]
    .join("\n")
}

fn fails(fixture: &Fixture, dir: &Utf8Path, args: &[&str]) -> String {
    let output = fixture.newgit(dir, args);
    assert!(
        !output.status.success(),
        "newgit {args:?} should have failed:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn not_a_member(fixture: &Fixture) {
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .not_a_member
        .insert("jack/store".to_owned());
}

#[test]
fn a_member_joins_and_ends_where_newgit_clone_ends() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "joined");
    assert!(!dir.join("apps/t3code").exists(), "the mirror withholds it");
    let mirrored_head = git(&dir, &["rev-parse", "HEAD"]);

    let stdout = fixture.ok(&dir, &["init"]);
    assert!(stdout.contains("Joined `jack/store`"), "{stdout}");

    let cloned = temp.join("cloned");
    fixture.ok(
        &temp,
        &[
            "clone",
            "jack/store",
            cloned.as_str(),
            "--url",
            &fixture.remote.url,
        ],
    );
    let head = git(&cloned, &["rev-parse", "HEAD"]);
    assert_ne!(mirrored_head, head, "the mirror's ids are not the view's");
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(
        git(&dir, &["log", "--format=%H"]),
        git(&cloned, &["log", "--format=%H"])
    );
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(dir.join("apps/t3code/app.ts")).expect("read"),
        "app\n"
    );
    assert_eq!(
        git(&dir, &["remote", "get-url", "origin"]),
        git(&cloned, &["remote", "get-url", "origin"])
    );
    assert!(fixture.ok(&dir, &["tracker", "list"]).contains("t3code"));
    assert_eq!(
        git(
            &dir,
            &["rev-list", "--count", "refs/newgit/trackers/t3code/main"]
        ),
        "1",
        "a snapshot, as a clone has"
    );

    // And it works: an instance's commit under the mount reaches the remote.
    fixture.ok(&dir, &["spawn", "work"]);
    let workspace = Utf8PathBuf::from(fixture.ok(&dir, &["status", "work", "--path"]).trim());
    std::fs::write(workspace.join("apps/t3code/mine.ts"), "mine\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "work in t3code"]);
    fixture.ok(&dir, &["merge", "work", "--into", "main"]);
    assert!(files_at(&fixture.remote.repo("jack.store.t.t3code"), "main").contains("mine.ts"));

    let again = fixture.ok(&dir, &["init"]);
    assert!(again.contains("nothing to do"), "{again}");
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
}

#[test]
fn running_init_again_after_joining_changes_nothing() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "joined");
    fixture.ok(&dir, &["init"]);
    let before = state(&dir);

    let stdout = fixture.ok(&dir, &["init"]);
    assert!(stdout.contains("nothing to do"), "{stdout}");
    assert_eq!(state(&dir), before);
}

#[test]
fn not_logged_in_stops_with_nothing_changed() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "stopped");
    let before = state(&dir);

    std::fs::remove_file(temp.join("home/.newgit/auth.toml")).expect("log out");
    let stderr = fails(&fixture, &dir, &["init"]);
    assert!(stderr.contains("newgit login"), "{stderr}");
    assert!(stderr.contains("newgit init"), "{stderr}");
    assert_eq!(state(&dir), before);
}

#[test]
fn a_local_commit_stops_it_naming_the_commit() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "ahead");
    std::fs::write(dir.join("local.txt"), "mine\n").expect("write");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "a commit only here"]);
    let before = state(&dir);

    let stderr = fails(&fixture, &dir, &["init"]);
    assert!(stderr.contains("a commit only here"), "{stderr}");
    assert_eq!(state(&dir), before);
}

#[test]
fn uncommitted_changes_stop_it_naming_them() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "dirty");
    std::fs::write(dir.join("README.md"), "edited\n").expect("write");
    std::fs::write(dir.join("new.txt"), "new\n").expect("write");
    let before = state(&dir);

    let stderr = fails(&fixture, &dir, &["init"]);
    assert!(
        stderr.contains("README.md") && stderr.contains("new.txt"),
        "{stderr}"
    );
    assert_eq!(state(&dir), before);
    assert_eq!(
        std::fs::read_to_string(dir.join("README.md")).expect("read"),
        "edited\n"
    );
}

#[test]
fn a_clone_behind_main_joins() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "behind");
    let cloned = temp.join("later");
    fixture.ok(
        &temp,
        &[
            "clone",
            "jack/store",
            cloned.as_str(),
            "--url",
            &fixture.remote.url,
        ],
    );
    fixture.ok(&cloned, &["spawn", "later"]);
    let workspace = Utf8PathBuf::from(fixture.ok(&cloned, &["status", "later", "--path"]).trim());
    std::fs::write(workspace.join("later.txt"), "later\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "later"]);
    fixture.ok(&cloned, &["merge", "later", "--into", "main"]);

    let stdout = fixture.ok(&dir, &["init"]);
    assert!(stdout.contains("Joined"), "{stdout}");
    assert!(dir.join("later.txt").exists());
    assert!(dir.join("apps/t3code/app.ts").exists());
}

#[test]
fn a_non_member_gets_a_copy_that_says_it_is_not_linked_and_acts_as_if_it_has_no_remote() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);

    // Their own fork of the mirror, which they cloned.
    let fork = temp.join("fork.git");
    git(
        &temp,
        &["clone", "-q", "--bare", mirror.as_str(), fork.as_str()],
    );
    let dir = github_clone(&fixture, &fork, "outsider");
    not_a_member(&fixture);
    let origin = git(&dir, &["remote", "get-url", "origin"]);
    let root_repo = fixture.remote.repo("jack.store.t.root");
    let remote_main = git(&root_repo, &["rev-parse", "main"]);
    let mirrored_tip = git(&mirror, &["rev-parse", "main"]);

    let stdout = fixture.ok(&dir, &["init"]);
    assert!(stdout.contains("NOT LINKED"), "{stdout}");
    assert!(stdout.contains("mounted trackers are not here"), "{stdout}");
    assert_eq!(git(&dir, &["remote", "get-url", "origin"]), origin);
    assert_eq!(
        git_output(&dir, &["config", "--get", "remote.origin.pushurl"])
            .status
            .code(),
        Some(1),
        "pushes go where they always did"
    );

    let shown = fixture.ok(&dir, &["remote", "show"]);
    assert!(shown.contains("NOT LINKED"), "{shown}");
    let json: Value =
        serde_json::from_str(fixture.ok(&dir, &["remote", "show", "--json"]).trim()).expect("json");
    assert_eq!(json["linked"], false);
    let refused = fixture.newgit(&dir, &["members", "list"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not linked"));
    let refused = fixture.newgit(&dir, &["remote", "add"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not linked"));

    // `newgit merge main` runs, and there is no remote for it to move.
    std::fs::write(dir.join("local.txt"), "mine\n").expect("write");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "local work"]);
    fixture.ok(&dir, &["merge", "main"]);
    assert_eq!(
        git(&root_repo, &["rev-parse", "main"]),
        remote_main,
        "the remote's `main` did not move"
    );

    // `git push origin main` reaches their fork, as in any repository.
    let tip = git(&dir, &["rev-parse", "main"]);
    git(&dir, &["push", "-q", "origin", "main"]);
    assert_eq!(git(&fork, &["rev-parse", "main"]), tip);

    // So does a push from an instance.
    fixture.ok(&dir, &["spawn", "local-work"]);
    let workspace = Utf8PathBuf::from(fixture.ok(&dir, &["status", "local-work", "--path"]).trim());
    std::fs::write(workspace.join("more.txt"), "more\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "more"]);
    git(&workspace, &["push", "-q", "origin", "HEAD"]);
    assert_eq!(
        git(&fork, &["rev-parse", "refs/heads/local-work"]),
        git(&workspace, &["rev-parse", "HEAD"])
    );

    // An export would force the mirror onto ids of its own making.
    let stderr = fails(&fixture, &dir, &["export", "--remote", "github"]);
    assert!(stderr.contains("not linked"), "{stderr}");
    assert_eq!(git(&mirror, &["rev-parse", "main"]), mirrored_tip);
    assert_eq!(git(&root_repo, &["rev-parse", "main"]), remote_main);
}

#[test]
fn an_unlinked_copy_joins_once_its_owner_is_a_member() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "later-member");
    not_a_member(&fixture);
    fixture.ok(&dir, &["init"]);
    let still = fixture.ok(&dir, &["init"]);
    assert!(still.contains("NOT LINKED"), "{still}");

    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .not_a_member
        .clear();
    let stdout = fixture.ok(&dir, &["init"]);
    assert!(stdout.contains("Joined"), "{stdout}");
    assert!(dir.join("apps/t3code/app.ts").exists());
    assert!(!dir.join(".newgit/local/unlinked").exists());
    assert_eq!(
        git(&dir, &["remote", "get-url", "origin"]),
        format!("{}/jack/store.git", fixture.remote.url),
        "linked, not just unmarked"
    );
    fixture.ok(&dir, &["spawn", "after-joining"]);
    assert_eq!(
        git(&dir, &["rev-parse", "HEAD"]),
        git(&fixture.store, &["rev-parse", "HEAD"])
    );
    assert!(
        fixture
            .ok(&dir, &["remote", "show"])
            .contains("jack/store on"),
        "linked now"
    );
}

#[test]
fn the_project_the_config_came_from_has_nothing_to_join() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    mirrored(&fixture);
    let before = git(&fixture.store, &["rev-parse", "HEAD"]);

    let stdout = fixture.ok(&fixture.store, &["init"]);
    assert!(stdout.contains("nothing to do"), "{stdout}");
    assert_eq!(git(&fixture.store, &["rev-parse", "HEAD"]), before);
}

/// A pull request merged on the mirror, not landed on the project.
fn merged_on_mirror(fixture: &Fixture, mirror: &Utf8Path) {
    let elsewhere = github_clone(fixture, mirror, "pull-request");
    std::fs::write(elsewhere.join("pr.txt"), "pr\n").expect("write");
    git(&elsewhere, &["add", "-A"]);
    git(&elsewhere, &["commit", "-q", "-m", "merged on the mirror"]);
    git(&elsewhere, &["push", "-q", "origin", "main"]);
}

#[test]
fn a_mirror_ahead_of_the_project_joins_and_says_so() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    merged_on_mirror(&fixture, &mirror);
    let dir = github_clone(&fixture, &mirror, "ahead-mirror");

    let stdout = fixture.ok(&dir, &["init"]);
    assert!(stdout.contains("Joined"), "{stdout}");
    assert!(
        stdout.contains("the mirror has 1 commit the project has not landed yet")
            && stdout.contains("`newgit pull --remote github`"),
        "{stdout}"
    );
    assert_eq!(
        git(&dir, &["rev-parse", "HEAD"]),
        git(&fixture.store, &["rev-parse", "HEAD"]),
        "landing them is not `init`'s to do"
    );
    assert!(!dir.join("pr.txt").exists());
    assert!(dir.join("apps/t3code/app.ts").exists());
}

#[test]
fn a_local_commit_on_a_mirror_ahead_still_stops_it_naming_only_that() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    merged_on_mirror(&fixture, &mirror);
    let dir = github_clone(&fixture, &mirror, "both");
    std::fs::write(dir.join("local.txt"), "mine\n").expect("write");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "a commit only here"]);
    let before = state(&dir);

    let stderr = fails(&fixture, &dir, &["init"]);
    assert!(stderr.contains("a commit only here"), "{stderr}");
    assert!(!stderr.contains("merged on the mirror"), "{stderr}");
    assert_eq!(state(&dir), before);
}

#[test]
fn an_ignored_file_the_view_would_overwrite_stops_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "ignored");
    std::fs::create_dir_all(dir.join("apps/t3code")).expect("mkdir");
    std::fs::write(dir.join("apps/t3code/app.ts"), "my own\n").expect("write");
    std::fs::write(dir.join(".git/info/exclude"), "apps/\n").expect("write");
    let before = state(&dir);

    let stderr = fails(&fixture, &dir, &["init"]);
    assert!(stderr.contains("apps/t3code/app.ts"), "{stderr}");
    assert_eq!(state(&dir), before);
    assert_eq!(
        std::fs::read_to_string(dir.join("apps/t3code/app.ts")).expect("read"),
        "my own\n"
    );
}

#[test]
fn an_ignored_file_matching_the_view_is_no_reason_to_stop() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = mirrored(&fixture);
    let dir = github_clone(&fixture, &mirror, "same");
    std::fs::create_dir_all(dir.join("apps/t3code")).expect("mkdir");
    std::fs::write(dir.join("apps/t3code/app.ts"), "app\n").expect("write");
    std::fs::write(dir.join(".git/info/exclude"), "apps/\n").expect("write");

    let stdout = fixture.ok(&dir, &["init"]);
    assert!(stdout.contains("Joined"), "{stdout}");
}
