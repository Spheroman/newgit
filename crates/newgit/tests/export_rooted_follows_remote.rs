//! A target rooted at a tracker exports that tracker's head as the remote
//! has it. A change lands at its anchor, so on the remote the tracker can be
//! ahead of anything on this machine, and of root, which has not rolled up
//! to it yet (docs/design/anchoring.md, build order step 6). The export
//! fetches the tracker's `main`, which the remote moves after each landing.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn a_rooted_export_takes_the_trackers_head_from_the_remote() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    std::fs::create_dir_all(fixture.store.join("apps/b")).expect("mkdir");
    std::fs::write(fixture.store.join("apps/b/index.js"), "b v1\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "apps/b"]);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "b", "--git", "--path", "apps/b"],
    );
    let mirror = fixture.temp.join("apps-b.git");
    git(
        &fixture.temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    let dir = fixture.store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("apps-b.toml"),
        format!("url = \"{mirror}\"\nroot = \"b\"\n"),
    )
    .expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "export apps-b"]);
    fixture.ok(&fixture.store, &["merge", "main"]);

    // Someone else's change landed at `b` on the remote: `b`'s head moved,
    // root's has not rolled up to it, and nothing here has seen it.
    let repo = fixture.remote.repo("jack.store.t.b");
    let blob = {
        let mut hash = Command::new("git")
            .args(["-C", repo.as_str(), "hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("git runs");
        hash.stdin
            .take()
            .expect("stdin")
            .write_all(b"b v2\n")
            .expect("write");
        String::from_utf8_lossy(&hash.wait_with_output().expect("hash").stdout)
            .trim()
            .to_owned()
    };
    let main = git(&repo, &["rev-parse", "main"]);
    let mut entries = git(&repo, &["ls-tree", "main"]);
    entries = entries
        .lines()
        .map(|line| {
            if line.ends_with("\tindex.js") {
                format!("100644 blob {blob}\tindex.js")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tree = {
        let mut make = Command::new("git")
            .args(["-C", repo.as_str(), "mktree"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("git runs");
        make.stdin
            .take()
            .expect("stdin")
            .write_all(format!("{entries}\n").as_bytes())
            .expect("write");
        String::from_utf8_lossy(&make.wait_with_output().expect("mktree").stdout)
            .trim()
            .to_owned()
    };
    let ahead = git(
        &repo,
        &["commit-tree", &tree, "-p", &main, "-m", "b v2, landed at b"],
    );
    git(&repo, &["update-ref", "refs/heads/main", &ahead]);
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .branches
        .get_mut("jack/store")
        .expect("project")
        .get_mut("main")
        .expect("main")["b"] = json!(ahead);

    fixture.ok(&fixture.store, &["export", "--remote", "apps-b"]);
    assert_eq!(
        git(&mirror, &["show", "main:index.js"]),
        "b v2",
        "the remote's head, exported"
    );
}
