//! `newgit remote delete`: a project leaves the remote whole, and its name
//! can be linked again from scratch.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn delete_wants_the_project_named_and_leaves_everything_when_it_is_not() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let output = fixture.newgit(&fixture.store, &["remote", "delete", "store"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("newgit remote delete jack/store"),
        "{stderr}"
    );
    assert!(fixture.remote.repo("jack.store.t.root").exists());
    let config =
        std::fs::read_to_string(fixture.store.join(".newgit/config.toml")).expect("config");
    assert!(config.contains("[remote]"), "{config}");
}

#[test]
fn delete_takes_every_repo_and_the_link_and_the_name_can_be_linked_again() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);

    let out = fixture.ok(&fixture.store, &["remote", "delete", "jack/store"]);
    assert!(out.contains("Deleted `jack/store`"), "{out}");
    let left: Vec<String> = std::fs::read_dir(&fixture.remote.repos)
        .expect("repos")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("jack.store."))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    let config =
        std::fs::read_to_string(fixture.store.join(".newgit/config.toml")).expect("config");
    assert!(!config.contains("[remote]"), "{config}");

    let out = fixture.ok(
        &fixture.store,
        &["remote", "add", "--url", &fixture.remote.url],
    );
    assert!(out.contains("Created `jack/store`"), "{out}");
    assert!(files_at(&fixture.remote.repo("jack.store.t.root"), "main").contains("README.md"));
}
