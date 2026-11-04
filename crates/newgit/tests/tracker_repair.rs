//! `newgit tracker repair`, through the real binary against the fake remote:
//! a split made before newgit kept message endings is rewritten here, in the
//! tracker store, in the composed-state notes, and on the remote — the
//! tracker repo and the branch's list — exactly as it is split now.

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// `commit` as newgit split it before message endings were kept: same tree,
/// parents, and identity; the message trimmed, trailers on, one newline.
fn as_split_before(repo: &Utf8Path, commit: &str) -> String {
    let raw = git(repo, &["cat-file", "commit", commit]);
    let (_, message) = raw.split_once("\n\n").expect("message");
    let (body, trailers) = message.rsplit_once("\n\n").expect("trailers");
    let old = format!("{body}\n\n{}\n", trailers.trim_end_matches('\n'));
    let tree = git(repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let parent = git(repo, &["rev-parse", &format!("{commit}^")]);
    let identity = |role: &str| {
        let value = raw
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{role} ")))
            .expect("identity");
        let (name, rest) = value.split_once(" <").expect("name");
        let (email, date) = rest.split_once("> ").expect("email");
        (name.to_owned(), email.to_owned(), format!("@{date}"))
    };
    let (author, author_email, author_date) = identity("author");
    let (committer, committer_email, committer_date) = identity("committer");
    let mut child = Command::new("git")
        .args(["-C", repo.as_str(), "commit-tree", "--no-gpg-sign", &tree])
        .args(["-p", &parent, "-F", "-"])
        .env("GIT_AUTHOR_NAME", author)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_AUTHOR_DATE", author_date)
        .env("GIT_COMMITTER_NAME", committer)
        .env("GIT_COMMITTER_EMAIL", committer_email)
        .env("GIT_COMMITTER_DATE", committer_date)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(old.as_bytes())
            .expect("write");
    }
    let output = child.wait_with_output().expect("git runs");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn root_list(fixture: &Fixture) -> String {
    fixture.remote.state.lock().expect("state").branches["jack/store"]["main"]["root"]
        .as_str()
        .expect("root's commit")
        .to_owned()
}

#[test]
fn repair_rewrites_an_old_split_everywhere_it_lives() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let store = &fixture.store;
    let trackers = store.join(".newgit/local/trackers.git");

    // A commit as GitHub writes a merge's message: no final newline.
    std::fs::write(store.join("README.md"), "merged\n").expect("write");
    git(store, &["add", "README.md"]);
    let tree = git(store, &["write-tree"]);
    let head = git(store, &["rev-parse", "HEAD"]);
    let mut child = Command::new("git")
        .args(["-C", store.as_str(), "commit-tree", "--no-gpg-sign", &tree])
        .args(["-p", &head, "-F", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"Merge it (#1)")
            .expect("write");
    }
    let merged = String::from_utf8_lossy(&child.wait_with_output().expect("git").stdout)
        .trim()
        .to_owned();
    git(store, &["reset", "-q", "--soft", &merged]);
    fixture.ok(store, &["merge", "main"]);
    let split = root_list(&fixture);
    assert_eq!(split_source(store, &split), merged);

    // Age it: everywhere that split commit is, put the old-style one.
    let old = as_split_before(store, &split);
    let remote_root = fixture.remote.repo("jack.store.t.root");
    git(
        store,
        &[
            "push",
            "-q",
            "--force",
            remote_root.as_str(),
            &format!("{old}:refs/heads/main"),
        ],
    );
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .branches
        .get_mut("jack/store")
        .expect("project")
        .get_mut("main")
        .expect("main")["root"] = old.clone().into();
    git(
        store,
        &[
            "push",
            "-q",
            trackers.as_str(),
            &format!("{old}:refs/heads/aged"),
        ],
    );
    for repo in [store, &trackers] {
        for reference in git(
            repo,
            &["for-each-ref", "--points-at", &split, "--format=%(refname)"],
        )
        .lines()
        {
            git(repo, &["update-ref", reference, &old]);
        }
    }
    git(&trackers, &["update-ref", "-d", "refs/heads/aged"]);
    let note = git(
        store,
        &[
            "notes",
            "--ref",
            "refs/notes/newgit-composed",
            "show",
            &merged,
        ],
    );
    git(
        store,
        &[
            "notes",
            "--ref",
            "refs/notes/newgit-composed",
            "add",
            "-f",
            "-m",
            &note.replace(&split, &old),
            &merged,
        ],
    );

    let dry = fixture.ok(store, &["tracker", "repair", "--dry-run"]);
    assert!(dry.contains("Would rewrite 1 tracker commit"), "{dry}");
    assert!(dry.contains("would move `main`"), "{dry}");
    assert_eq!(root_list(&fixture), old, "a dry run moves nothing");

    let repaired = fixture.ok(store, &["tracker", "repair"]);
    assert!(repaired.contains("Rewrote 1 tracker commit"), "{repaired}");
    assert_eq!(
        root_list(&fixture),
        split,
        "main's list names the commit split now"
    );
    assert_eq!(git(&remote_root, &["rev-parse", "main"]), split);
    assert_eq!(
        git(&trackers, &["rev-parse", "refs/trackers/root/main"]),
        split
    );
    assert!(
        git(
            store,
            &[
                "notes",
                "--ref",
                "refs/notes/newgit-composed",
                "show",
                &merged
            ]
        )
        .contains(&format!("root {split}")),
        "the composed-state note names it too"
    );

    let again = fixture.ok(store, &["tracker", "repair"]);
    assert!(again.contains("Nothing to repair"), "{again}");
}
