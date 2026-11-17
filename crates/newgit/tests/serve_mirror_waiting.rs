// A pull refused only for carrying a refused pull's commits is not lost when
// that refusal's advice is followed (#451). Pull A is refused (an instance's
// withheld work), and pull B, a later pull request on top of it, waits on A.
// The instance lands, then:
//
// - A is sent again, as its refusal says: A lands, and B, whose own commits
//   have not, stays said, on its own, saying what to do. Sent again, B
//   lands, and the trouble is over.
// - B is sent again first: it carries A, so both land, and A sent again
//   after finds nothing.

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

/// A later pull request merged on the mirror, changing `crates/serve/b.rs`.
fn later_merged(home: &Utf8Path, work: &Utf8Path) -> String {
    git(home, work, &["checkout", "-q", "-b", "later"]);
    std::fs::write(work.join("crates/serve/b.rs"), "fn b() { 4 }\n").expect("write");
    git(home, work, &["commit", "-q", "-am", "b returns four"]);
    git(home, work, &["checkout", "-q", "main"]);
    git(
        home,
        work,
        &[
            "merge",
            "-q",
            "--no-ff",
            "later",
            "-m",
            "Merge pull request #4 from jack/later",
        ],
    );
    git(home, work, &["push", "-q", "origin", "main"]);
    git(home, work, &["rev-parse", "HEAD"])
}

fn trouble(state: &Value) -> Vec<Value> {
    state["ended"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|one| matches!(one["outcome"].as_str(), Some("refused" | "gave up")))
        .collect()
}

/// The project mirrored, `worker`'s withheld work merged on the mirror (A,
/// refused) and a later pull request on top of it (B, waiting on A), and
/// then `worker` landed here: A's refusal's own way on.
struct Waiting {
    temp: Utf8PathBuf,
    server: Server,
    token: String,
    a: String,
    b: String,
    _guard: TempDir,
}

fn waiting() -> Waiting {
    let (guard, temp) = tempdir();
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
    let a = workers_branch_merged(&home, &work);
    let (status, answer) = deliver(&server, "push", "d-a", &push_event(&a, "jack"));
    assert_eq!(status, 202, "{answer}");
    pulls_done(&server, &token);
    let b = later_merged(&home, &work);
    let (status, answer) = deliver(&server, "push", "d-b", &push_event(&b, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    let said = trouble(&done);
    assert_eq!(said.len(), 1, "B is said on A: {done}");
    assert_eq!(said[0]["commit"], a.as_str(), "{done}");
    assert!(
        said[0]["why"]
            .as_str()
            .is_some_and(|why| why.contains("1 later pull")),
        "{done}"
    );

    newgit(&home, &store, &["merge", "worker"]);
    Waiting {
        temp,
        server,
        token,
        a,
        b,
        _guard: guard,
    }
}

#[test]
fn the_refused_pull_sent_again_lands_and_the_one_waiting_on_it_stays_said() {
    let w = waiting();
    let (status, answer) = deliver(&w.server, "push", "d-a", &push_event(&w.a, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&w.server, &w.token);
    assert_eq!(
        main_file(&w.server, &w.token, &w.temp, "README.md"),
        "store, by worker\n",
        "A landed"
    );
    assert_eq!(
        main_file(&w.server, &w.token, &w.temp, "crates/serve/b.rs"),
        "fn b() {}\n",
        "B did not"
    );
    // B is not dropped with A's refusal: it is said on its own, saying how
    // to bring it in.
    let said = trouble(&done);
    assert_eq!(said.len(), 1, "{done}");
    assert_eq!(said[0]["commit"], w.b.as_str(), "{done}");
    let why = said[0]["why"].as_str().unwrap_or_default();
    assert!(why.contains("has landed since"), "{why}");
    assert!(why.contains("redeliver the event"), "{why}");
    assert!(
        !why.contains("newgit merge worker"),
        "advice already taken: {why}"
    );

    // Sent again, B lands, and the trouble is over.
    let (status, answer) = deliver(&w.server, "push", "d-b", &push_event(&w.b, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&w.server, &w.token);
    assert!(trouble(&done).is_empty(), "{done}");
    assert_eq!(
        main_file(&w.server, &w.token, &w.temp, "crates/serve/b.rs"),
        "fn b() { 4 }\n"
    );
}

#[test]
fn the_waiting_pull_sent_again_first_brings_both_in() {
    let w = waiting();
    let (status, answer) = deliver(&w.server, "push", "d-b", &push_event(&w.b, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&w.server, &w.token);
    assert!(trouble(&done).is_empty(), "{done}");
    assert_eq!(
        main_file(&w.server, &w.token, &w.temp, "README.md"),
        "store, by worker\n"
    );
    assert_eq!(
        main_file(&w.server, &w.token, &w.temp, "crates/serve/b.rs"),
        "fn b() { 4 }\n"
    );

    // A, sent again after, has nothing left to bring in.
    let (status, answer) = deliver(&w.server, "push", "d-a", &push_event(&w.a, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&w.server, &w.token);
    assert!(trouble(&done).is_empty(), "{done}");
    assert_eq!(last_ended(&done)["outcome"], "nothing", "{done}");
}
