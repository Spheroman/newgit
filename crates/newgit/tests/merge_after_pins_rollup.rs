//! An instance merges again after root rolled its last merge up as a `pins`
//! snapshot: a commit of root's own, not descending from the merger's, so
//! the composed `main` holds none of the instance's commits. Merged in the
//! view, that met an old base and "conflicted" on files only the instance
//! had changed (seen on the dev remote). Merged tracker by tracker, as it
//! is when root's own files are untouched, it goes through.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn a_second_merge_after_a_pins_roll_up_goes_through() {
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
    git(&fixture.store, &["add", "-A"]);
    git(
        &fixture.store,
        &["commit", "-q", "--allow-empty", "-m", "docs is a tracker"],
    );
    fixture.ok(&fixture.store, &["merge", "main"]);

    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    let root = fixture.remote.repo("jack.store.t.root");
    let edit = |text: &str| {
        std::fs::write(workspace.join("docs/guide.md"), text).expect("write");
        git(&workspace, &["add", "-A"]);
        git(&workspace, &["commit", "-q", "-m", text.trim()]);
    };

    edit("first\n");
    let before = git(&root, &["rev-parse", "main"]);
    fixture.ok(&fixture.store, &["merge", "feature-a"]);

    // As a `pins` parent rolls it up: root's tree as merged, but a commit of
    // root's own on the head before, keeping none of the merger's history.
    let tree = git(&root, &["rev-parse", "main^{tree}"]);
    let snapshot = git(
        &root,
        &["commit-tree", &tree, "-p", &before, "-m", "Roll up `docs`"],
    );
    git(&root, &["update-ref", "refs/heads/main", &snapshot]);
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .branches
        .get_mut("jack/store")
        .expect("project")
        .get_mut("main")
        .expect("main")["root"] = json!(snapshot);

    edit("second\n");
    let merged = fixture.newgit(&fixture.store, &["merge", "feature-a"]);
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    let docs = fixture.remote.repo("jack.store.t.docs");
    assert_eq!(git(&docs, &["show", "main:guide.md"]), "second");
}
