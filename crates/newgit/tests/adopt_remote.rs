//! A project whose history already holds `.newgit/` — a plain `git clone`,
//! adopted with `newgit init` — linked to a newgit remote. `access` carves
//! out of `project`, which carves out of `root`; carved in the wrong order,
//! `access` had no history yet when the root's work was split, and `remote
//! add` failed merging unrelated histories.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn an_adopted_clone_links_with_every_code_tracker_carved_and_the_same_commits() {
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
    assert!(access.contains(".newgit/trackers/root.toml"), "{access}");
    let state = fixture.remote.state.lock().expect("state");
    let list = &state.branches["jack/adopted"]["main"];
    assert_eq!(
        list["root"], head,
        "root's main is the adopted history itself"
    );
}
