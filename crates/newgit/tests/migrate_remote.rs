//! `newgit tracker migrate` on a project linked to a remote, through the
//! real binary and the fake remote: the remote as the old engine left it
//! (`project` in the list, `access`'s files at their workspace paths, a
//! synced checkpoint) moved onto pins — root's `main` the carve's root
//! commit, `main`'s list the new index exactly, every checkpoint naming root
//! commits — with a dry run that touches nothing and a second run that
//! writes nothing. And a merge into a branch other than `main` leaves every
//! child's `main` alone.

#![allow(dead_code)]

include!("support/fake_remote.rs");
include!("support/old_layout.rs");

#[test]
fn the_migration_moves_the_remote_onto_pins_and_a_dry_run_touches_nothing() {
    let (_guard, temp) = tempdir();
    let old = old_project(&temp);
    let fixture = &old.fixture;
    let before = fixture.remote.requests().len();
    let dry = fixture.ok(&fixture.store, &["tracker", "migrate", "--dry-run"]);
    assert!(dry.contains("Gate: recomposed"), "{dry}");
    assert_eq!(
        fixture.remote.requests().len(),
        before,
        "a dry run touches no remote route: {:?}",
        &fixture.remote.requests()[before..]
    );

    let out = fixture.ok(&fixture.store, &["tracker", "migrate"]);
    assert!(out.contains("On jack/store"), "{out}");

    // Root's `main` is the carve's root commit, `access`'s its pin.
    let tracker_store = fixture.store.join(".newgit/local/trackers.git");
    let local_root = git(&tracker_store, &["rev-parse", "refs/trackers/root/main"]);
    let root_repo = fixture.remote.repo("jack.store.t.root");
    assert_eq!(git(&root_repo, &["rev-parse", "main"]), local_root);
    assert_eq!(
        split_source(&root_repo, "main"),
        git(&fixture.store, &["rev-parse", "HEAD"]),
        "the carve, split"
    );
    let pin = git(&root_repo, &["rev-parse", "main:.newgit/pins/access"]);
    assert_eq!(
        git(
            &fixture.remote.repo("jack.store.t.access"),
            &["rev-parse", "main"]
        ),
        pin
    );

    // `main`'s list is the new index exactly.
    let list = fixture.remote.state.lock().expect("state").branches["jack/store"]["main"].clone();
    assert_eq!(list, json!({ "root": local_root, "access": pin }));

    // Every checkpoint names root commits only, and root's fork has them.
    let local = CheckpointLog::new(fixture.store.join(".newgit/checkpoints/feature"), "feature")
        .load("ckpt_001")
        .expect("local record");
    for (side, code) in [("head", &local.code.head), ("tip", &local.code.tip)] {
        assert_eq!(
            code.keys().collect::<Vec<_>>(),
            vec!["root"],
            "local {side}: {code:?}"
        );
        assert_eq!(code["root"], old.feature);
    }
    let remote = fixture.remote.state.lock().expect("state").checkpoints
        [&("feature".to_owned(), "ckpt_001".to_owned())]
        .clone();
    assert_eq!(remote["code"]["head"], json!({ "root": old.feature }));
    assert_eq!(remote["code"]["tip"], json!({ "root": old.feature }));
    let fork = fixture.remote.repo("jack.store.i.feature.root");
    assert_eq!(
        git(&fork, &["rev-parse", "refs/heads/feature"]),
        old.feature
    );

    // A second run writes nothing anywhere.
    let before = writes(fixture).len();
    let again = fixture.ok(&fixture.store, &["tracker", "migrate"]);
    assert!(again.contains("Nothing to migrate"), "{again}");
    assert_eq!(
        writes(fixture).len(),
        before,
        "{:?}",
        &writes(fixture)[before..]
    );
    let _ = old.main;
}

#[test]
fn a_merge_into_a_branch_other_than_main_moves_no_childs_main() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    std::fs::create_dir_all(fixture.store.join("docs")).expect("mkdir");
    std::fs::write(fixture.store.join("docs/guide.md"), "guide\n").expect("write");
    git(&fixture.store, &["add", "docs"]);
    git(&fixture.store, &["commit", "-q", "-m", "docs"]);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "docs", "--git", "--path", "docs"],
    );
    fixture.ok(&fixture.store, &["merge", "main"]);
    let docs = fixture.remote.repo("jack.store.t.docs");
    let docs_main = git(&docs, &["rev-parse", "main"]);
    // A second branch, as `main` is now.
    {
        let mut state = fixture.remote.state.lock().expect("state");
        let branches = state.branches.get_mut("jack/store").expect("project");
        let list = branches["main"].clone();
        branches.insert("release".to_owned(), list);
    }

    fixture.ok(&fixture.store, &["spawn", "fix"]);
    let workspace = workspace_of(&fixture, "fix");
    std::fs::write(workspace.join("docs/guide.md"), "a better guide\n").expect("write");
    git(&workspace, &["commit", "-qam", "better docs"]);
    let out = fixture.ok(&fixture.store, &["merge", "fix", "--into", "release"]);
    assert!(out.contains("docs"), "{out}");

    let release =
        fixture.remote.state.lock().expect("state").branches["jack/store"]["release"].clone();
    let pinned = release["docs"]
        .as_str()
        .expect("docs on release")
        .to_owned();
    assert_ne!(pinned, docs_main, "release pins the new docs commit");
    assert_eq!(
        git(&docs, &["rev-parse", "main"]),
        docs_main,
        "docs's own `main` is `main`'s to move, not release's"
    );
    assert_eq!(
        git(&docs, &["rev-parse", "release"]),
        pinned,
        "its commit is kept on docs's own `release`"
    );
    assert_eq!(
        git(
            &fixture.store.join(".newgit/local/trackers.git"),
            &["rev-parse", "refs/trackers/docs/main"]
        ),
        docs_main,
        "and not here either"
    );
}
