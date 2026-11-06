//! `newgit clone` fetches root whole and only the commits it pins of each
//! mounted tracker; a child's history arrives once, the first time something
//! really needs it (docs/design/trackers.md, *Snapshots*).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

const HISTORY_LINE: &str = "fetching t3code's history from";

/// t3code, three commits long, mounted at `apps/t3code`, merged into `main`
/// and exported rooted to a bare repository. Returns that repository.
fn published(fixture: &Fixture) -> Utf8PathBuf {
    let upstream = fixture.temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "newgit"]);
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

fn clone(fixture: &Fixture, name: &str, flags: &[&str]) -> (Utf8PathBuf, String) {
    let dir = fixture.temp.join(name);
    let mut args = vec!["clone", "jack/store", dir.as_str(), "--url"];
    args.push(&fixture.remote.url);
    args.extend_from_slice(flags);
    let output = fixture.newgit(&fixture.temp, &args);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "clone failed:\n{}\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    (dir, stderr)
}

/// Run newgit, and return its stdout and stderr, requiring success.
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

fn store_of(clone: &Utf8Path) -> Utf8PathBuf {
    clone.join(".newgit/local/trackers.git")
}

/// How many commits of t3code this project holds.
fn local_commits(clone: &Utf8Path) -> usize {
    git(
        clone,
        &["rev-list", "--count", "refs/newgit/trackers/t3code/main"],
    )
    .trim()
    .parse()
    .expect("a count")
}

fn snapshot_held(clone: &Utf8Path) -> bool {
    !newgit_core::snapshots::held(clone)
        .expect("held")
        .get("t3code")
        .is_none_or(std::collections::BTreeSet::is_empty)
}

/// A commit on t3code's `main` on the remote that root does not pin: as a
/// GitHub merge landing through the webhook does. Returns it.
fn moved_on_remote(fixture: &Fixture, file: &str) -> String {
    let repo = fixture.remote.repo("jack.store.t.t3code");
    let elsewhere = fixture.temp.join(format!("elsewhere-{file}"));
    git(
        &fixture.temp,
        &[
            "clone",
            "-q",
            "-b",
            "main",
            repo.as_str(),
            elsewhere.as_str(),
        ],
    );
    std::fs::write(elsewhere.join(file), "upstream\n").expect("write");
    git(&elsewhere, &["add", "-A"]);
    git(&elsewhere, &["commit", "-q", "-m", "upstream work"]);
    git(&elsewhere, &["push", "-q", "origin", "main"]);
    git(&elsewhere, &["rev-parse", "HEAD"])
}

/// An instance of `clone` with one commit under the mount.
fn commit_in_child(fixture: &Fixture, clone: &Utf8Path, name: &str) -> Utf8PathBuf {
    run(fixture, clone, &["spawn", name]);
    let workspace = Utf8PathBuf::from(run(fixture, clone, &["status", name, "--path"]).0.trim());
    std::fs::write(workspace.join("apps/t3code/mine.ts"), "mine\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "work in t3code"]);
    workspace
}

#[test]
fn a_snapshot_clone_composes_the_same_ids_as_a_full_clone() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);
    let (full, _) = clone(&fixture, "full", &["--full"]);

    let head = git(&full, &["rev-parse", "HEAD"]);
    assert_eq!(git(&snapshot, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&snapshot, &["log", "--format=%H"]),
        git(&full, &["log", "--format=%H"]),
        "the whole view, commit for commit"
    );
    assert_eq!(git(&snapshot, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(snapshot.join("apps/t3code/app.ts")).expect("read"),
        "app 3\n"
    );

    // An ordinary project: a workspace spawns, whole, from it.
    run(&fixture, &snapshot, &["spawn", "plain"]);
    let workspace = Utf8PathBuf::from(
        run(&fixture, &snapshot, &["status", "plain", "--path"])
            .0
            .trim(),
    );
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&workspace, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
    assert!(
        git_output(&workspace, &["fsck", "--connectivity-only"])
            .status
            .success()
    );
}

#[test]
fn a_clone_fetches_no_childs_history() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (snapshot, stderr) = clone(&fixture, "snapshot", &[]);

    assert!(!stderr.contains("history from"), "{stderr}");
    assert_eq!(local_commits(&snapshot), 1, "only the pinned commit");
    let pin = git(
        &snapshot,
        &["rev-parse", "refs/newgit/trackers/t3code/main"],
    );
    let repo = fixture.remote.repo("jack.store.t.t3code");
    assert_eq!(pin, git(&repo, &["rev-parse", "main"]));
    let parent = git(&repo, &["rev-parse", "main~1"]);
    assert!(
        !git_output(&snapshot, &["cat-file", "-e", &parent])
            .status
            .success(),
        "its parent is not here"
    );

    // Root is whole, and the store is not shallow at all.
    let root = fixture.remote.repo("jack.store.t.root");
    assert_eq!(
        git(
            &snapshot,
            &["rev-list", "--count", "refs/newgit/trackers/root/main"]
        ),
        git(&root, &["rev-list", "--count", "main"])
    );
    let store = store_of(&snapshot);
    assert_eq!(
        git(&store, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
    assert_eq!(
        git(&store, &["for-each-ref", "refs/trackers/t3code/"]),
        "",
        "nothing of t3code in the store of whole histories"
    );
}

#[test]
fn an_instance_commit_under_a_mount_round_trips_with_no_history() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);
    let repo = fixture.remote.repo("jack.store.t.t3code");
    let pin = git(&repo, &["rev-parse", "main"]);

    let workspace = commit_in_child(&fixture, &snapshot, "work");
    let head = git(&workspace, &["rev-parse", "HEAD"]);
    let (_, checkpointed) = run(&fixture, &workspace, &["checkpoint"]);
    assert!(!checkpointed.contains("history from"), "{checkpointed}");
    let (_, merged) = run(&fixture, &snapshot, &["merge", "work", "--into", "main"]);
    assert!(!merged.contains("history from"), "{merged}");
    assert!(snapshot_held(&snapshot), "still a snapshot");

    // The remote's history is whole, on the pin.
    let tip = git(&repo, &["rev-parse", "main"]);
    assert_eq!(git(&repo, &["rev-parse", "main~1"]), pin);
    assert_eq!(git(&repo, &["rev-list", "--count", "main"]), "4");
    assert!(git(&repo, &["fsck", "--connectivity-only"]).is_empty());
    assert!(files_at(&repo, "main").contains("mine.ts"));

    // Anyone else's clone composes it the same.
    let (other, _) = clone(&fixture, "other", &["--full"]);
    let (same, _) = clone(&fixture, "same", &[]);
    assert_eq!(
        git(&other, &["rev-parse", "HEAD"]),
        git(&same, &["rev-parse", "HEAD"])
    );
    assert_eq!(git(&other, &["log", "-1", "--format=%s"]), "work in t3code");
    assert_eq!(git(&same, &["rev-parse", "HEAD"]), head, "the very commit");
    assert_eq!(
        git(&same, &["rev-parse", "refs/newgit/trackers/t3code/main"]),
        tip
    );
}

#[test]
fn a_checkpoint_spawns_on_a_snapshot_clone_with_no_history() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (author, _) = clone(&fixture, "author", &[]);
    let workspace = commit_in_child(&fixture, &author, "shared");
    let head = git(&workspace, &["rev-parse", "HEAD"]);
    run(&fixture, &workspace, &["checkpoint"]);

    let (reader, _) = clone(&fixture, "reader", &[]);
    let (spawned, stderr) = run(
        &fixture,
        &reader,
        &[
            "spawn",
            "review",
            "--from-checkpoint",
            "shared/ckpt_001",
            "--json",
        ],
    );
    assert!(!stderr.contains("history from"), "{stderr}");
    let outcome: Value = serde_json::from_str(spawned.trim()).expect("json");
    let at = Utf8PathBuf::from(outcome["workspace"].as_str().expect("workspace"));
    assert_eq!(git(&at, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(at.join("apps/t3code/mine.ts")).expect("read"),
        "mine\n"
    );
    assert_eq!(local_commits(&reader), 1, "no history came with it");
    assert_eq!(
        git(&at, &["log", "-1", "--format=%s"]),
        "work in t3code",
        "the checkpoint's own commit"
    );
    assert!(
        snapshot_held(&reader),
        "and no push fell back to fetching everything"
    );
}

#[test]
fn the_next_ordinary_fetch_does_not_deepen_a_snapshot() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);

    run(&fixture, &snapshot, &["spawn", "root-work"]);
    let workspace = Utf8PathBuf::from(
        run(&fixture, &snapshot, &["status", "root-work", "--path"])
            .0
            .trim(),
    );
    std::fs::write(workspace.join("rooted.txt"), "root\n").expect("write");
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "work in root"]);
    let (_, merged) = run(
        &fixture,
        &snapshot,
        &["merge", "root-work", "--into", "main"],
    );
    assert!(!merged.contains("history from"), "{merged}");
    assert_eq!(local_commits(&snapshot), 1);
    assert!(snapshot_held(&snapshot));
}

#[test]
fn a_new_pin_merged_elsewhere_composes_on_a_snapshot_clone() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (first, _) = clone(&fixture, "first", &[]);
    let (second, _) = clone(&fixture, "second", &[]);

    let workspace = commit_in_child(&fixture, &first, "theirs");
    run(&fixture, &first, &["merge", "theirs", "--into", "main"]);
    let their_head = git(&workspace, &["rev-parse", "HEAD"]);

    // The second clone's own work merges beside it: root moved both ways, so
    // the merge composes a pin this clone has never seen.
    run(&fixture, &second, &["spawn", "ours"]);
    let ours = Utf8PathBuf::from(
        run(&fixture, &second, &["status", "ours", "--path"])
            .0
            .trim(),
    );
    std::fs::write(ours.join("ours.txt"), "ours\n").expect("write");
    git(&ours, &["add", "-A"]);
    git(&ours, &["commit", "-q", "-m", "work beside it"]);
    let (_, merged) = run(&fixture, &second, &["merge", "ours", "--into", "main"]);
    assert!(!merged.contains("history from"), "{merged}");

    let (fresh, _) = clone(&fixture, "fresh", &["--full"]);
    assert_eq!(
        git(
            &fresh,
            &["merge-base", "--is-ancestor", &their_head, "HEAD"]
        ),
        ""
    );
    assert!(fresh.join("apps/t3code/mine.ts").exists());
    assert!(fresh.join("ours.txt").exists());
}

#[test]
fn a_real_merge_in_a_child_fetches_its_history_then_succeeds() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);
    let repo = fixture.remote.repo("jack.store.t.t3code");
    let workspace = commit_in_child(&fixture, &snapshot, "work");
    let upstream = moved_on_remote(&fixture, "upstream.ts");

    let (_, merged) = run(&fixture, &snapshot, &["merge", "work", "--into", "main"]);
    assert!(merged.contains(HISTORY_LINE), "{merged}");
    assert!(!snapshot_held(&snapshot));

    // t3code's `main` is a real merge of its two lines, and has every file.
    let parents = git(&repo, &["log", "-1", "--format=%P", "main"]);
    assert!(parents.contains(&upstream), "{parents}");
    assert_eq!(parents.split_whitespace().count(), 2, "{parents}");
    let files = files_at(&repo, "main");
    assert!(files.contains("upstream.ts") && files.contains("mine.ts"));
    assert_eq!(
        git(&repo, &["merge-base", &upstream, "main~1"]).len(),
        40,
        "a real base"
    );
    assert!(git_output(&workspace, &["status"]).status.success());
}

#[test]
fn a_fast_forward_pull_needs_no_history() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);

    let (stdout, stderr) = run(&fixture, &snapshot, &["pull", "t3code"]);
    assert!(stdout.contains("up to date"), "{stdout}");
    assert!(!stderr.contains("history from"), "{stderr}");

    moved_on_remote(&fixture, "one.ts");
    moved_on_remote(&fixture, "two.ts");
    let (stdout, stderr) = run(&fixture, &snapshot, &["pull", "t3code"]);
    assert!(!stderr.contains("history from"), "{stderr}");
    assert!(stdout.contains("2 commits"), "{stdout}");
    assert!(snapshot.join("apps/t3code/two.ts").exists());
}

#[test]
fn a_follow_ref_behind_the_pin_fetches_history_and_does_not_move_it_back() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);
    let head = git(&snapshot, &["rev-parse", "HEAD"]);

    // `main` on the remote is now an ancestor of the pin. On a snapshot
    // Git would say it is not, and the pin would move backwards.
    let repo = fixture.remote.repo("jack.store.t.t3code");
    git(&repo, &["branch", "kept", "main"]);
    git(&repo, &["update-ref", "refs/heads/main", "main~1"]);

    let (stdout, stderr) = run(&fixture, &snapshot, &["pull", "t3code"]);
    assert!(stderr.contains(HISTORY_LINE), "{stderr}");
    assert!(stdout.contains("up to date"), "{stdout}");
    assert_eq!(git(&snapshot, &["rev-parse", "HEAD"]), head);
}

#[test]
fn a_rooted_export_works_from_a_snapshot_clone() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = published(&fixture);
    let tip = git(&github, &["rev-parse", "newgit"]);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);

    let (_, stderr) = run(&fixture, &snapshot, &["export", "--remote", "t3code"]);
    assert!(stderr.contains(HISTORY_LINE), "{stderr}");
    assert_eq!(
        git(&github, &["rev-parse", "newgit"]),
        tip,
        "nothing rewrote the mirror, and it is not shallow"
    );
    assert!(git(&github, &["fsck", "--connectivity-only"]).is_empty());
}

#[test]
fn pull_remote_from_a_rooted_target_works_from_a_snapshot_clone() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let github = published(&fixture);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);

    let pr = fixture.temp.join("pr");
    git(
        &fixture.temp,
        &["clone", "-q", "-b", "newgit", github.as_str(), pr.as_str()],
    );
    std::fs::write(pr.join("pr.ts"), "pr\n").expect("write");
    git(&pr, &["add", "-A"]);
    git(&pr, &["commit", "-q", "-m", "t3: from a PR"]);
    git(&pr, &["push", "-q", "origin", "newgit"]);

    let (stdout, stderr) = run(&fixture, &snapshot, &["pull", "--remote", "t3code"]);
    assert!(stderr.contains(HISTORY_LINE), "{stderr}");
    assert!(stdout.contains("Pulled 1 commit"), "{stdout}");
    assert!(snapshot.join("apps/t3code/pr.ts").exists());
}

#[test]
fn a_trackers_log_fetches_its_history_once() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (snapshot, _) = clone(&fixture, "snapshot", &[]);

    let (log, stderr) = run(&fixture, &snapshot, &["log", "t3code", "--", "--format=%s"]);
    assert_eq!(stderr.matches(HISTORY_LINE).count(), 1, "{stderr}");
    assert_eq!(log.lines().collect::<Vec<_>>(), ["t3: 3", "t3: 2", "t3: 1"]);

    // It is here for good: nothing fetches it again.
    let (again, stderr) = run(&fixture, &snapshot, &["log", "t3code", "--", "--format=%s"]);
    assert_eq!(again, log);
    assert!(!stderr.contains("history from"), "{stderr}");
    moved_on_remote(&fixture, "later.ts");
    let (_, stderr) = run(&fixture, &snapshot, &["pull", "t3code"]);
    assert!(!stderr.contains("history from"), "{stderr}");

    assert_eq!(
        git(
            &store_of(&snapshot),
            &["rev-list", "--count", "refs/trackers/t3code/main"]
        ),
        "3",
        "the store has it too"
    );
    assert!(
        git_output(&snapshot, &["fsck", "--connectivity-only"])
            .status
            .success()
    );
}

#[test]
fn full_fetches_every_history_up_front_as_a_clone_always_did() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    published(&fixture);
    let (full, stderr) = clone(&fixture, "full", &["--full"]);

    assert!(!stderr.contains("history from"), "{stderr}");
    assert_eq!(
        git(&full, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
    assert_eq!(git(&full, &["for-each-ref", "refs/newgit/snapshots/"]), "");
    assert_eq!(local_commits(&full), 3);
    assert_eq!(
        git(
            &store_of(&full),
            &["rev-list", "--count", "refs/trackers/t3code/main"]
        ),
        "3"
    );

    // Nothing here ever asks for history.
    let workspace = commit_in_child(&fixture, &full, "work");
    moved_on_remote(&fixture, "upstream.ts");
    let (_, merged) = run(&fixture, &full, &["merge", "work", "--into", "main"]);
    assert!(!merged.contains("history from"), "{merged}");
    assert!(git_output(&workspace, &["status"]).status.success());
}
