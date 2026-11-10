// `newgit pr create` and `newgit pr checkout` against the real `newgit
// serve`: an author proposes an instance's pushed work, a reviewer spawns it
// by number with its uncommitted work, the pull request follows the next
// push, and `newgit merge` marks it merged as it lands.

#![allow(
    dead_code,
    reason = "the shared helpers include some this file does not use"
)]

include!("support/serve.rs");

/// `newgit` that is expected to fail: its standard error.
fn refused(home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> String {
    let output = run(env!("CARGO_BIN_EXE_newgit"), home, dir, args);
    assert!(
        !output.status.success(),
        "newgit {args:?} succeeded:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
}

/// jack's project on `server`, with ana a member and her clone of it.
fn project(server: &Server, temp: &Utf8Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let (jack, ana) = (temp.join("jack"), temp.join("ana"));
    login(server, &jack, "jack");
    login(server, &ana, "ana");
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&jack, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    git(&jack, &store, &["add", "."]);
    git(&jack, &store, &["commit", "-q", "-m", "initial"]);
    newgit(&jack, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    git(&jack, &store, &["add", "-A"]);
    git(&jack, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(&jack, &store, &["remote", "add", "--url", &server.url]);
    newgit(&jack, &store, &["members", "add", "ana"]);
    let ana_clone = temp.join("ana-store");
    newgit(
        &ana,
        temp,
        &[
            "clone",
            "jack/store",
            ana_clone.as_str(),
            "--url",
            &server.url,
        ],
    );
    (store, ana_clone)
}

#[test]
fn a_reviewer_checks_out_a_pull_request_and_merge_closes_it() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (jack, ana) = (temp.join("jack"), temp.join("ana"));
    let (store, ana_clone) = project(&server, &temp);

    newgit(&jack, &store, &["spawn", "feature"]);
    let workspace = Utf8PathBuf::from(newgit(&jack, &store, &["status", "feature", "--path"]));
    std::fs::write(workspace.join("README.md"), "hello\nchanged\n").expect("write");
    git(&jack, &workspace, &["commit", "-q", "-am", "first change"]);
    git(&jack, &workspace, &["push", "-q"]);

    // A commit the remote does not have yet: proposing it is refused.
    std::fs::write(workspace.join("README.md"), "hello\nchanged\nagain\n").expect("write");
    git(
        &jack,
        &workspace,
        &[
            "commit",
            "-q",
            "-am",
            "Greet twice",
            "-m",
            "Because once was not enough.",
        ],
    );
    let said = refused(&jack, &workspace, &["pr", "create"]);
    assert!(said.contains("push it first"), "{said}");

    // Pushed, with uncommitted work along: title and body from the commit.
    std::fs::write(workspace.join("notes.txt"), "wip\n").expect("write");
    git(&jack, &workspace, &["push", "-q"]);
    let agent_commit = git(&jack, &workspace, &["rev-parse", "HEAD"]);
    let opened = json(&newgit(&jack, &workspace, &["pr", "create", "--json"]));
    assert_eq!(opened["number"], 1, "{opened}");
    assert_eq!(opened["title"], "Greet twice");
    assert_eq!(opened["body"], "Because once was not enough.");
    assert_eq!(opened["state"], "open");
    assert_eq!(opened["instance"], "feature");
    assert_eq!(opened["baseRefName"], "main");
    assert_eq!(opened["author"], "jack");
    assert_eq!(opened["head"]["commit"], agent_commit.as_str());
    let first_head = opened["head"]["checkpoint"]
        .as_str()
        .expect("a head")
        .to_owned();
    assert!(first_head.starts_with("feature/"), "{first_head}");

    // One open pull request per instance and branch.
    let said = refused(&jack, &store, &["pr", "create", "feature", "-t", "again"]);
    assert!(
        said.contains("already has pull request #1 open against `main`"),
        "{said}"
    );

    // ana finds it, and checks it out by number: the same commit, and
    // jack's uncommitted work.
    let listed = newgit(&ana, &ana_clone, &["pr", "list"]);
    assert!(
        listed.contains("#1") && listed.contains("Greet twice"),
        "{listed}"
    );
    let checked = newgit(&ana, &ana_clone, &["pr", "checkout", "1"]);
    assert!(
        checked.contains("Pull request #1: Greet twice"),
        "{checked}"
    );
    let review = Utf8PathBuf::from(newgit(&ana, &ana_clone, &["status", "pr-1", "--path"]));
    assert_eq!(git(&ana, &review, &["rev-parse", "HEAD"]), agent_commit);
    assert_eq!(
        std::fs::read_to_string(review.join("notes.txt")).expect("notes"),
        "wip\n"
    );
    let said = refused(&ana, &ana_clone, &["pr", "checkout", "1"]);
    assert!(
        said.contains("an instance `pr-1` exists here already"),
        "{said}"
    );

    // jack pushes again: the pull request's head follows, and a second
    // checkout gets the new commit.
    std::fs::write(workspace.join("README.md"), "hello\nreviewed\n").expect("write");
    git(
        &jack,
        &workspace,
        &["commit", "-q", "-am", "address review"],
    );
    git(&jack, &workspace, &["push", "-q"]);
    let moved_commit = git(&jack, &workspace, &["rev-parse", "HEAD"]);
    let again = json(&newgit(
        &ana,
        &ana_clone,
        &["pr", "checkout", "1", "--name", "pr-1-again", "--json"],
    ));
    assert_eq!(again["pull"], 1, "{again}");
    assert_eq!(again["name"], "pr-1-again");
    assert_ne!(again["from"].as_str(), Some(first_head.as_str()));
    let review = Utf8PathBuf::from(newgit(
        &ana,
        &ana_clone,
        &["status", "pr-1-again", "--path"],
    ));
    assert_eq!(git(&ana, &review, &["rev-parse", "HEAD"]), moved_commit);

    // The merge lands it and marks it merged, in one step.
    let merged = newgit(&jack, &store, &["merge", "feature"]);
    assert!(merged.contains("pull request #1: merged"), "{merged}");
    let open = newgit(&ana, &ana_clone, &["pr", "list"]);
    assert!(open.contains("No open pull requests"), "{open}");
    let all = json(&newgit(
        &ana,
        &ana_clone,
        &["pr", "list", "--state", "merged", "--json"],
    ));
    assert_eq!(all[0]["number"], 1, "{all}");
    assert_eq!(all[0]["mergedBy"], "jack");
    assert!(
        all[0].get("body").is_none(),
        "a list leaves bodies out: {all}"
    );
}

#[test]
fn pull_requests_are_refused_for_what_they_would_reveal_or_cannot_hold() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let jack = temp.join("jack");
    let (store, _) = project(&server, &temp);

    // Never pushed: nothing on the remote to propose.
    newgit(&jack, &store, &["spawn", "draft-work"]);
    let said = refused(&jack, &store, &["pr", "create", "draft-work"]);
    assert!(said.contains("push it first"), "{said}");

    // Private: a pull request would show it to the project.
    newgit(&jack, &store, &["spawn", "embargo", "--private"]);
    let workspace = Utf8PathBuf::from(newgit(&jack, &store, &["status", "embargo", "--path"]));
    std::fs::write(workspace.join("README.md"), "fixed\n").expect("write");
    git(
        &jack,
        &workspace,
        &["commit", "-q", "-am", "fix it quietly"],
    );
    git(&jack, &workspace, &["push", "-q"]);
    let said = refused(&jack, &workspace, &["pr", "create"]);
    assert!(said.contains("`embargo` is private"), "{said}");

    // A branch that is not there.
    newgit(&jack, &store, &["spawn", "feature"]);
    let workspace = Utf8PathBuf::from(newgit(&jack, &store, &["status", "feature", "--path"]));
    std::fs::write(workspace.join("README.md"), "changed\n").expect("write");
    git(&jack, &workspace, &["commit", "-q", "-am", "change"]);
    git(&jack, &workspace, &["push", "-q"]);
    let said = refused(&jack, &workspace, &["pr", "create", "--base", "nope"]);
    assert!(said.contains("no branch `nope`"), "{said}");

    // A draft opens as one; removing its instance closes it.
    let opened = json(&newgit(
        &jack,
        &workspace,
        &[
            "pr", "create", "--draft", "-t", "Change", "-F", "-", "--json",
        ],
    ));
    assert_eq!(opened["isDraft"], true, "{opened}");
    assert_eq!(opened["body"], "", "{opened}");
    let listed = newgit(&jack, &store, &["pr", "list"]);
    assert!(listed.contains("draft"), "{listed}");
    newgit(&jack, &store, &["remove", "feature"]);
    let closed = json(&newgit(
        &jack,
        &store,
        &["pr", "list", "--state", "closed", "--json"],
    ));
    assert_eq!(closed[0]["number"], opened["number"], "{closed}");
    let said = refused(&jack, &store, &["pr", "checkout", "1"]);
    assert!(said.contains("has no head"), "{said}");
}
