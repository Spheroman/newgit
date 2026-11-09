//! A view rooted at a child tracker (`/<owner>/<name>/<child>.git`) composes
//! the same commits whatever was composed before it, and expands its own
//! children at every commit of its history, not only at the tip.
//!
//! Found on newgit's own project: `t3code`'s view, composed in fresh stores,
//! lacked `.repos` at the commit that created it, and was cached that way;
//! composed after root's view it 500ed or came out right. Snapshotting the
//! history's pins named them from `root` instead of the view's root, and
//! deciding whether to fetch the view root's whole history counted the
//! one-commit-deep snapshots root's view had left.
//!
//! These run the real `newgit endpoint-compose` against the fake remote's
//! tracker repos, as the Worker's endpoint container does.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

const PROJECT: &str = "jack/store";

/// Publish the store's `main` to the fake remote.
fn publish(fixture: &Fixture) {
    git(&fixture.store, &["add", "-A"]);
    let _ = git_output(&fixture.store, &["commit", "-q", "-m", "trackers"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
}

/// Compose `main`'s view rooted at `root` into `dir`, as the endpoint
/// container would, and return the composed bare repo.
fn compose(fixture: &Fixture, dir: &Utf8Path, root: &str) -> Utf8PathBuf {
    let repos = &fixture.remote.repos;
    let prefix = PROJECT.replace('/', ".");
    let tip = git(
        &fixture.remote.repo(&format!("{prefix}.t.root")),
        &["rev-parse", "main"],
    );
    let list = index_of(repos, PROJECT, &tip);
    let mut plan_repos = serde_json::Map::new();
    for tracker in list.keys().map(String::as_str).chain(["access"]) {
        plan_repos.insert(
            tracker.to_owned(),
            json!({
                "url": repos.join(format!("{prefix}.t.{tracker}.git")),
                "auth": "Authorization: Basic eDpw",
            }),
        );
    }
    let plan = json!({
        "dir": dir,
        "branches": { "main": list },
        "default_branch": "main",
        "repos": plan_repos,
        "root": root,
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .arg("endpoint-compose")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("newgit runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(plan.to_string().as_bytes())
        .expect("write the plan");
    let output = child.wait_with_output().expect("compose");
    assert!(
        output.status.success(),
        "endpoint-compose rooted at `{root}` failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Utf8PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

/// A project whose `lib` tracker has a child, `vendor`, that moved twice
/// after it was carved, so `lib`'s history pins three different commits of it.
fn nested_project(temp: &Utf8Path) -> Fixture {
    let fixture = setup(temp);
    let store = &fixture.store;
    std::fs::create_dir_all(store.join("apps/lib/vendor")).expect("mkdir");
    std::fs::write(store.join("apps/lib/a.txt"), "a\n").expect("write");
    std::fs::write(store.join("apps/lib/vendor/v.txt"), "v1\n").expect("write");
    git(store, &["add", "."]);
    git(store, &["commit", "-q", "-m", "lib"]);
    fixture.ok(
        store,
        &["tracker", "create", "lib", "--git", "--path", "apps/lib"],
    );
    fixture.ok(
        store,
        &[
            "tracker",
            "create",
            "vendor",
            "--git",
            "--path",
            "apps/lib/vendor",
        ],
    );
    for version in ["v2", "v3"] {
        std::fs::write(store.join("apps/lib/vendor/v.txt"), format!("{version}\n")).expect("write");
        git(store, &["commit", "-q", "-am", version]);
    }
    publish(&fixture);
    fixture
}

/// The child's content at the commit of `lib`'s view whose subject is `subject`.
fn vendor_at(view: &Utf8Path, subject: &str) -> String {
    let commit = git(
        view,
        &["log", "--format=%H", &format!("--grep=^{subject}$"), "HEAD"],
    );
    assert!(
        !commit.is_empty() && !commit.contains('\n'),
        "one `{subject}`: {commit:?}"
    );
    git(view, &["show", &format!("{commit}:vendor/v.txt")])
}

#[test]
fn a_rooted_view_expands_its_children_at_every_commit_in_fresh_stores() {
    let (_guard, temp) = tempdir();
    let fixture = nested_project(&temp);

    let view = compose(&fixture, &temp.join("endpoint"), "lib");
    assert_eq!(
        vendor_at(&view, "v2"),
        "v2",
        "a historical pin was left unexpanded"
    );
    assert_eq!(vendor_at(&view, "v3"), "v3");
}

#[test]
fn a_rooted_view_is_the_same_whatever_was_composed_before_it() {
    let (_guard, temp) = tempdir();
    let fixture = nested_project(&temp);

    // In one container the rooted view first; in another, root's view first.
    let alone = compose(&fixture, &temp.join("alone"), "lib");
    compose(&fixture, &temp.join("after"), "root");
    let after = compose(&fixture, &temp.join("after"), "lib");
    assert_eq!(
        git(&alone, &["rev-parse", "HEAD"]),
        git(&after, &["rev-parse", "HEAD"]),
        "the same view composed to different commits"
    );
}

#[test]
fn a_rooted_view_after_roots_reads_the_whole_history_not_roots_snapshots() {
    // Root pins only the tip of a tracker imported with its own history, so
    // composing root's view leaves that tip in the snapshot store one commit
    // deep. The rooted view after it must still fetch the whole history
    // rather than walk from that snapshot, which ends at the cut.
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let upstream = temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "main"]);
    for version in ["one", "two", "three"] {
        std::fs::write(upstream.join("lib.txt"), format!("{version}\n")).expect("write");
        git(&upstream, &["add", "."]);
        git(&upstream, &["commit", "-q", "-m", version]);
    }
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "lib",
            "--git",
            "--from",
            upstream.as_str(),
            "--mount",
            "apps/lib",
        ],
    );
    publish(&fixture);

    let dir = temp.join("endpoint");
    compose(&fixture, &dir, "root");
    let view = compose(&fixture, &dir, "lib");
    assert_eq!(
        git(&view, &["rev-parse", "HEAD"]),
        git(&upstream, &["rev-parse", "HEAD"])
    );
    assert_eq!(git(&view, &["rev-list", "--count", "HEAD"]), "3");
}
