// Where a webhook pull on `newgit serve` fetches from, and what it does not
// pull at all (#414).
//
// - A pull fetches only from the repository its signed event named. `main`'s
//   definitions can change between the event and the pull (the pull waits
//   out a failure, a deploy, another pull), and a target that now names
//   another repository is not fetched from: the event vouched for the first
//   one's commits, and the URL a definition writes is never handed to Git.
// - The server's own export coming back as an event is answered at the
//   door, whoever GitHub says pushed it (the server's token: an App's is no
//   member's login). Nothing is queued, and nothing is looked for.
// - The queue is rows in SQLite: a pull waiting out a failure when the
//   server stops is taken up by the next one, and lands (#353).

include!("support/serve.rs");
include!("support/serve_mirror.rs");

/// One commit on the mirror's `main`, pushed there directly: the commit.
fn pushed_to_mirror(home: &Utf8Path, work: &Utf8Path, text: &str) -> String {
    std::fs::write(work.join("README.md"), text).expect("write");
    git(home, work, &["commit", "-q", "-am", "on the mirror"]);
    git(home, work, &["push", "-q", "origin", "main"]);
    git(home, work, &["rev-parse", "HEAD"])
}

/// `/state` once its oldest pull has failed at least once.
fn pull_failing(server: &Server, token: &str) -> Value {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = state(server, token);
        if now["pulls"][0]["attempts"].as_u64().unwrap_or(0) >= 1 {
            return now;
        }
        assert!(
            now["pulls"]
                .as_array()
                .is_some_and(|pulls| !pulls.is_empty()),
            "the pull ended instead of failing: {now}"
        );
        assert!(
            std::time::Instant::now() < until,
            "the pull never failed: {now}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[test]
fn a_pull_fetches_only_from_the_repository_its_event_named() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    let work = mirror_clone(&home, &temp, &mirror, "work");
    let pushed = pushed_to_mirror(&home, &work, "store, from the mirror\n");
    // Another repository holding the very same commits, and the mirror out
    // of reach for now: the pull fails, and waits to try again.
    let elsewhere = github.root.join("jack/elsewhere.git");
    git(
        &home,
        &temp,
        &["clone", "-q", "--bare", mirror.as_str(), elsewhere.as_str()],
    );
    let away = github.root.join("jack/mirror.away");
    std::fs::rename(&mirror, &away).expect("rename");
    let (status, answer) = deliver(&server, "push", "d-1", &push_event(&pushed, "jack"));
    assert_eq!(status, 202, "{answer}");
    pull_failing(&server, &token);

    // Meanwhile `main` points the target at the other repository.
    set_definition_export(
        &store,
        "root",
        Some("{ url = \"github.com/jack/elsewhere\", name = \"mirror\", branches = true }"),
    );
    git(&home, &store, &["commit", "-q", "-am", "the target moves"]);
    newgit(&home, &store, &["merge", "main"]);

    // The event was for jack/mirror: nothing is fetched from jack/elsewhere,
    // though it holds the commit and the target now names it.
    let done = pulls_done(&server, &token);
    let ended = last_ended(&done);
    assert_eq!(ended["outcome"], "gave up", "{done}");
    let why = ended["why"].as_str().unwrap_or_default();
    assert!(why.contains("no longer exports to"), "{why}");
    assert!(why.contains("jack/mirror"), "{why}");
    assert_eq!(main_file(&server, &token, &temp, "README.md"), "store\n");
}

#[test]
fn the_servers_own_export_coming_back_is_answered_without_a_pull() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let _store = mirrored_project(&temp, &home, &server);
    let tip = mirror_main_until(&mirror, |tip| !tip.is_empty());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while state(&server, &token)["exports"][0]["tip"] != tip.as_str() {
        assert!(
            std::time::Instant::now() < until,
            "the export never said its tip"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // GitHub says the server's token pushed it: an App's bot, no member.
    let (status, answer) = deliver(
        &server,
        "push",
        "d-own",
        &push_event(&tip, "newgit-exports[bot]"),
    );
    assert_eq!(status, 200, "{answer}");
    assert!(
        answer["ignored"]
            .as_str()
            .is_some_and(|why| why.contains("exported")),
        "{answer}"
    );
    let now = state(&server, &token);
    assert_eq!(
        now["pulls"],
        serde_json::json!([]),
        "nothing was queued: {now}"
    );
    assert_eq!(
        now["ended"],
        serde_json::json!([]),
        "nothing was looked for: {now}"
    );

    // Any other commit from that pusher is still refused: it is no member.
    let (status, answer) = deliver(
        &server,
        "push",
        "d-bot",
        &push_event(&"b".repeat(40), "newgit-exports[bot]"),
    );
    assert_eq!(status, 403, "{answer}");
}

#[test]
fn a_pull_waiting_when_the_server_stops_lands_after_it_starts_again() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let api = no_api();
    let server = start_mirrored(&temp, &github, &api);
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let _store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    let work = mirror_clone(&home, &temp, &mirror, "work");
    let pushed = pushed_to_mirror(&home, &work, "store, from the mirror\n");
    // GitHub is out of reach: the pull fails and waits.
    let away = github.root.join("jack/mirror.away");
    std::fs::rename(&mirror, &away).expect("rename");
    let (status, answer) = deliver(&server, "push", "d-1", &push_event(&pushed, "jack"));
    assert_eq!(status, 202, "{answer}");
    let failing = pull_failing(&server, &token);
    assert!(
        failing["pulls"][0]["lastError"].is_string(),
        "it says why: {failing}"
    );

    // A deploy: the server stops with the pull still queued, GitHub comes
    // back, and the next server takes the queue up where it was.
    drop(server);
    std::fs::rename(&away, &mirror).expect("rename");
    let server = start_mirrored(&temp, &github, &api);
    let before = state(&server, &token);
    let done = pulls_done(&server, &token);
    assert_eq!(
        last_ended(&done)["outcome"],
        "landed",
        "{before} then {done}"
    );
    assert_eq!(
        main_file(&server, &token, &temp, "README.md"),
        "store, from the mirror\n"
    );
}
