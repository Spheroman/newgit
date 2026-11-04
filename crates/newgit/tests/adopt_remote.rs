//! A project whose history already holds `.newgit/` — a plain `git clone`,
//! adopted with `newgit init` — linked to a newgit remote. Root adopts the
//! history up to the commit that first mounts `access`; split makes root
//! commits of the rest, pinning `access`, and gives back the same view
//! commits.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn an_adopted_clone_links_with_every_git_tracker_carved_and_the_same_commits() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);

    let clone = temp.join("clone");
    git(
        &temp,
        &["clone", "-q", fixture.store.as_str(), clone.as_str()],
    );
    // Not the original's link: the clone makes its own project.
    let config = clone.join(".newgit/config.toml");
    let text = std::fs::read_to_string(&config).expect("config");
    let text = text
        .split("[remote]")
        .next()
        .expect("before [remote]")
        .to_owned();
    std::fs::write(
        &config,
        text.replace("name = \"store\"", "name = \"adopted\""),
    )
    .expect("write");
    git(&clone, &["commit", "-q", "-am", "its own project"]);
    let head = git(&clone, &["rev-parse", "HEAD"]);

    let out = fixture.ok(&clone, &["init"]);
    assert!(out.contains("gave `root` this project's history"), "{out}");
    fixture.ok(&clone, &["remote", "add", "--url", &fixture.remote.url]);

    let access = files_at(&fixture.remote.repo("jack.adopted.t.access"), "main");
    assert!(access.contains("root.toml"), "{access}");
    let state = fixture.remote.state.lock().expect("state");
    let list = state.branches["jack/adopted"]["main"].clone();
    drop(state);
    assert_eq!(
        split_source(&fixture.remote.repo("jack.adopted.t.root"), "main"),
        head,
        "root's main is the adopted history, its last commit split from the clone's own"
    );
    assert!(list.get("access").is_some(), "{list:?}");
}
