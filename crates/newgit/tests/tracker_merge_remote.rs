//! `newgit tracker merge` on a shared tracker stored on the remote moves the
//! remote's `main` too: the tracker repo's branch `main`, as a code
//! tracker's is. A checkpoint's sync never pushes a shared tracker's `main`,
//! so without this a merge reached nobody but this machine.

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// `name` defined with `--storage <storage>`, owning `site`, and an
/// instance `landing` that has captured content for it.
fn captured(fixture: &Fixture, name: &str, audience: &str, storage: &str) -> Utf8PathBuf {
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            name,
            "--audience",
            audience,
            "--storage",
            storage,
        ],
    );
    fixture.ok(&fixture.store, &["tracker", "track", name, "site"]);
    fixture.ok(&fixture.store, &["spawn", "landing"]);
    let workspace = workspace_of(fixture, "landing");
    std::fs::create_dir_all(workspace.join("site")).expect("mkdir");
    std::fs::write(workspace.join("site/index.html"), "<h1>v1</h1>\n").expect("write");
    fixture.ok(&workspace, &["tracker", "capture", name]);
    workspace
}

fn local_main(fixture: &Fixture, tracker: &str) -> String {
    git(
        &fixture.store.join(".newgit/local/trackers.git"),
        &["rev-parse", &format!("refs/trackers/{tracker}/main")],
    )
}

#[test]
fn merging_a_shared_remote_tracker_moves_its_main_on_the_remote() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = captured(&fixture, "site", "project-devs", "remote");

    let output = fixture.ok(&workspace, &["tracker", "merge", "site"]);
    assert!(
        output.contains("`main` moved on jack/store too"),
        "{output}"
    );
    let repo = fixture.remote.repo("jack.store.t.site");
    assert_eq!(
        git(&repo, &["rev-parse", "refs/heads/main"]),
        local_main(&fixture, "site")
    );
    assert_eq!(git(&repo, &["show", "main:site/index.html"]), "<h1>v1</h1>");

    // A later merge moves it again, and a checkpoint alone does not.
    std::fs::write(workspace.join("site/index.html"), "<h1>v2</h1>\n").expect("write");
    fixture.ok(&workspace, &["tracker", "capture", "site"]);
    fixture.ok(&workspace, &["checkpoint"]);
    assert_eq!(
        git(&repo, &["show", "main:site/index.html"]),
        "<h1>v1</h1>",
        "a checkpoint's sync never moves a shared tracker's main"
    );
    fixture.ok(&workspace, &["tracker", "merge", "site"]);
    assert_eq!(git(&repo, &["show", "main:site/index.html"]), "<h1>v2</h1>");
    assert_eq!(
        git(&repo, &["rev-parse", "refs/heads/main"]),
        local_main(&fixture, "site")
    );
}

#[test]
fn merging_a_local_tracker_stays_on_this_machine() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = captured(&fixture, "notes", "project-devs", "local");
    let before = fixture.remote.requests().len();

    let output = fixture.ok(&workspace, &["tracker", "merge", "notes"]);
    assert!(!output.contains("remote:"), "{output}");
    assert_eq!(
        fixture.remote.requests().len(),
        before,
        "nothing asked of the remote"
    );
    assert!(!fixture.remote.repo("jack.store.t.notes").exists());
}
