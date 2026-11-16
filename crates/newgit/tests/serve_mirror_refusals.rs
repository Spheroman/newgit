// What `newgit status` says of refused webhook pulls on `newgit serve`, and
// how the trouble ends (#451 c).
//
// - A later pull that carries a refused pull's commits and is refused for
//   the same reason is said once, on the first. Refused for a reason of its
//   own (the first's cause is over, and now it conflicts), it says that
//   reason, and the first's advice, already taken, is not repeated.
// - The other way on a refusal names, `newgit pull --remote <target>` then
//   `newgit merge main`, ends the trouble with no event at all: the export
//   that then reaches the mirror says the mirror has nothing `main` lacks.

include!("support/serve.rs");
include!("support/serve_mirror.rs");

/// Instance `name` with `files` committed in its workspace and pushed.
fn instance_pushed(home: &Utf8Path, store: &Utf8Path, name: &str, files: &[(&str, &str)]) {
    newgit(home, store, &["spawn", name]);
    let workspace = Utf8PathBuf::from(newgit(home, store, &["status", name, "--path"]));
    for (path, text) in files {
        std::fs::write(workspace.join(path), text).expect("write");
        git(home, &workspace, &["add", path]);
    }
    git(
        home,
        &workspace,
        &["commit", "-q", "-m", &format!("{name}'s work")],
    );
    git(home, &workspace, &["push", "-q"]);
}

/// `worker`'s exported branch merged into the mirror's `main` with a merge
/// commit, as a pull request there: the merge.
fn workers_branch_merged(home: &Utf8Path, work: &Utf8Path) -> String {
    git(home, work, &["fetch", "-q", "origin"]);
    let branches = git(home, work, &["branch", "-r"]);
    let exported = branches
        .lines()
        .map(str::trim)
        .find(|branch| branch.contains("worker"))
        .unwrap_or_else(|| panic!("worker's branch was not exported: {branches}"))
        .trim_start_matches("origin/")
        .to_owned();
    git(home, work, &["checkout", "-q", "main"]);
    git(
        home,
        work,
        &[
            "merge",
            "-q",
            "--no-ff",
            &format!("origin/{exported}"),
            "-m",
            &format!("Merge pull request #3 from jack/{exported}"),
        ],
    );
    git(home, work, &["push", "-q", "origin", "main"]);
    git(home, work, &["rev-parse", "HEAD"])
}

fn refusals(state: &Value) -> Vec<Value> {
    state["ended"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|one| one["outcome"] == "refused")
        .collect()
}

#[test]
fn a_later_pull_refused_for_its_own_reason_says_that_reason() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    instance_pushed(
        &home,
        &store,
        "worker",
        &[
            ("README.md", "store, by worker\n"),
            ("apps/t3code/app.ts", "export const worker = 1\n"),
        ],
    );
    let work = mirror_clone(&home, &temp, &mirror, "pr");
    let first = workers_branch_merged(&home, &work);
    let (status, answer) = deliver(&server, "push", "d-1", &push_event(&first, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "refused", "{done}");

    // A later pull request on the mirror changes `crates/serve/b.rs`.
    git(&home, &work, &["checkout", "-q", "-b", "later"]);
    std::fs::write(work.join("crates/serve/b.rs"), "fn b() { 4 }\n").expect("write");
    git(&home, &work, &["commit", "-q", "-am", "b returns four"]);
    git(&home, &work, &["checkout", "-q", "main"]);
    git(
        &home,
        &work,
        &[
            "merge",
            "-q",
            "--no-ff",
            "later",
            "-m",
            "Merge pull request #4 from jack/later",
        ],
    );
    git(&home, &work, &["push", "-q", "origin", "main"]);
    let second = git(&home, &work, &["rev-parse", "HEAD"]);
    let later = push_event(&second, "jack");
    let (status, answer) = deliver(&server, "push", "d-2", &later);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    let said = refusals(&done);
    assert_eq!(said.len(), 1, "the same reason, said once: {done}");
    assert_eq!(said[0]["commit"], first.as_str(), "{done}");

    // The first refusal's way on is taken: `worker` lands. And `main` here
    // changes the same line of `b.rs` another way.
    newgit(&home, &store, &["merge", "worker"]);
    std::fs::write(store.join("crates/serve/b.rs"), "fn b() { 5 }\n").expect("write");
    git(&home, &store, &["commit", "-q", "-am", "b returns five"]);
    newgit(&home, &store, &["merge", "main"]);

    // The later event, sent again, is refused for its own reason now, and
    // that is what is said: not `worker`, whose work has landed.
    let (status, answer) = deliver(&server, "push", "d-2", &later);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    let said = refusals(&done);
    assert_eq!(said.len(), 1, "one refusal, the one that stands: {done}");
    assert_eq!(said[0]["commit"], second.as_str(), "{done}");
    let why = said[0]["why"].as_str().unwrap_or_default();
    assert!(why.contains("conflicts in"), "{why}");
    assert!(why.contains("crates/serve/b.rs"), "{why}");
    assert!(
        !why.contains("newgit merge worker"),
        "advice already taken: {why}"
    );
    assert!(!why.contains("it carries commit"), "its own reason: {why}");
    let status = newgit(&home, &store, &["status"]);
    assert_eq!(status.matches("was refused").count(), 1, "{status}");
    assert!(status.contains("conflicts in"), "{status}");
    assert!(!status.contains("newgit merge worker"), "{status}");
}

#[test]
fn pulling_the_mirror_by_hand_ends_a_refusal_once_the_export_reaches_it() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    instance_pushed(
        &home,
        &store,
        "worker",
        &[
            ("README.md", "store, by worker\n"),
            ("apps/t3code/app.ts", "export const worker = 1\n"),
        ],
    );
    let work = mirror_clone(&home, &temp, &mirror, "pr");
    let first = workers_branch_merged(&home, &work);
    let (status, answer) = deliver(&server, "push", "d-1", &push_event(&first, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    let ended = last_ended(&done);
    assert_eq!(ended["outcome"], "refused", "{done}");
    let why = ended["why"].as_str().unwrap_or_default();
    assert!(why.contains("newgit pull --remote mirror"), "{why}");
    let status = newgit(&home, &store, &["status"]);
    assert!(status.contains("was refused"), "{status}");

    // The second way on it names: bring the mirror in by hand. (What that
    // does with `worker`'s withheld work is the client's pull's to decide,
    // and not what this is about.)
    newgit(&home, &store, &["pull", "--remote", "mirror"]);
    newgit(&home, &store, &["merge", "main"]);
    assert_eq!(
        main_file(&server, &token, &temp, "README.md"),
        "store, by worker\n"
    );

    // No event is sent. The export finds the mirror's merge in `main`, and
    // that ends the trouble.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let now = state(&server, &token);
        if now["exports"][0]["state"] == "up to date" && refusals(&now).is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "the refusal is still said, or the export never caught up: {now}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let tip = git(&home, &mirror, &["rev-parse", "refs/heads/main"]);
    assert!(
        ancestor(&mirror, &first, &tip),
        "the mirror's merge is kept"
    );
    let status = newgit(&home, &store, &["status"]);
    assert!(!status.contains("was refused"), "{status}");
}

#[test]
fn after_the_instance_merges_the_mirrors_merge_lands_and_the_export_follows() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    instance_pushed(
        &home,
        &store,
        "worker",
        &[
            ("README.md", "store, by worker\n"),
            ("apps/t3code/app.ts", "export const worker = 1\n"),
        ],
    );
    let work = mirror_clone(&home, &temp, &mirror, "pr");
    let first = workers_branch_merged(&home, &work);
    let event = push_event(&first, "jack");
    let (status, answer) = deliver(&server, "push", "d-1", &event);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "refused", "{done}");

    // The first way on: land the instance, then send the event again.
    newgit(&home, &store, &["merge", "worker"]);
    let (status, answer) = deliver(&server, "push", "d-1", &event);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert!(
        matches!(
            last_ended(&done)["outcome"].as_str(),
            Some("landed" | "nothing")
        ),
        "{done}"
    );

    // `main` now holds the mirror's merge as itself, though its sides
    // differ in the tracker the mirror does not ship: the export finds the
    // mirror where it is, and the next landing fast-forwards it.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let now = state(&server, &token);
        if now["exports"][0]["state"] == "up to date" {
            assert_eq!(now["exports"][0]["tip"], first.as_str(), "{now}");
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "the export never caught up: {now}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    std::fs::write(store.join("crates/serve/b.rs"), "fn b() { 6 }\n").expect("write");
    git(&home, &store, &["commit", "-q", "-am", "b returns six"]);
    newgit(&home, &store, &["merge", "main"]);
    let tip = mirror_main_until(&mirror, |tip| tip != first);
    assert!(
        ancestor(&mirror, &first, &tip),
        "a fast-forward from the mirror's merge"
    );
    let status = newgit(&home, &store, &["status"]);
    assert!(!status.contains("was refused"), "{status}");
    assert!(!status.contains("is failing"), "{status}");
}
