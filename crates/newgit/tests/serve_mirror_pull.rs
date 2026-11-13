// `newgit serve`'s mirrors (#414): it exports each target's view to its
// mirror after a landing, and lands what is merged there through the
// webhook, as a merge. GitHub is bare repositories on disk.
//
// - The export leaves out the tracker the target withholds, and the
//   client's own `newgit export` of the same `main` pushes nothing new: one
//   composition, the same ids wherever it is made.
// - #451 (a): a pull request rebuilt from the mirror's `main` (one new
//   commit, not the old ones rebased) that changes a tracker mounted below
//   root, merged there while `main` here moved the same file elsewhere and
//   the withheld tracker too, lands through the server's merge without a
//   spurious conflict; the export then fast-forwards the mirror from the
//   merged pull request, its commits kept.
// - A push event for a commit that is on `main` already ends as `nothing`.

include!("support/serve.rs");
include!("support/serve_mirror.rs");

#[test]
fn the_export_withholds_what_it_does_not_ship_and_composes_the_clients_ids() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);

    let tip = mirror_main_until(&mirror, |tip| !tip.is_empty());
    let files = git(&home, &mirror, &["ls-tree", "-r", "--name-only", &tip]);
    assert!(files.contains("crates/serve/a.rs"), "{files}");
    assert!(files.contains("README.md"), "{files}");
    assert!(
        !files.contains("apps/t3code"),
        "a withheld tracker reached the mirror: {files}"
    );

    // The client composes the very same commits: nothing new to push.
    newgit(&home, &store, &["export", "--remote", "mirror"]);
    assert_eq!(git(&home, &mirror, &["rev-parse", "refs/heads/main"]), tip);

    let now = state(&server, &token);
    let export = &now["exports"][0];
    assert_eq!(export["target"], "mirror", "{now}");
    assert_eq!(export["state"], "up to date", "{now}");
    assert_eq!(export["tip"], tip.as_str(), "{now}");

    // A push of what the mirror has already is our own export coming back.
    let (status, answer) = deliver(&server, "push", "d-own", &push_event(&tip, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "nothing", "{done}");
}

#[test]
fn a_branch_rebuilt_from_the_mirrors_main_lands_without_a_spurious_conflict() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    let exported = mirror_main_until(&mirror, |tip| !tip.is_empty());

    // On the mirror: a pull request rebuilt from `main` as one commit,
    // changing the end of `crates/serve/a.rs`, merged with a merge commit.
    let work = mirror_clone(&home, &temp, &mirror, "pr");
    git(&home, &work, &["checkout", "-q", "-b", "fix"]);
    let a = std::fs::read_to_string(work.join("crates/serve/a.rs")).expect("a.rs");
    std::fs::write(
        work.join("crates/serve/a.rs"),
        a.replace("fn five() {}", "fn five() { 5 }"),
    )
    .expect("write");
    git(&home, &work, &["commit", "-q", "-am", "five returns five"]);
    git(&home, &work, &["checkout", "-q", "main"]);
    git(
        &home,
        &work,
        &[
            "merge",
            "-q",
            "--no-ff",
            "fix",
            "-m",
            "Merge pull request #1 from jack/fix",
        ],
    );
    let merged = git(&home, &work, &["rev-parse", "HEAD"]);
    git(&home, &work, &["push", "-q", "origin", "main"]);

    // Meanwhile here: the start of the same file, the withheld tracker, and
    // root. The export cannot follow (the mirror has the merge), and says so.
    let a = std::fs::read_to_string(store.join("crates/serve/a.rs")).expect("a.rs");
    std::fs::write(
        store.join("crates/serve/a.rs"),
        a.replace("fn one() {}", "fn one() { 1 }"),
    )
    .expect("write");
    std::fs::write(store.join("apps/t3code/app.ts"), "export const t3 = 1\n").expect("write");
    std::fs::write(store.join("README.md"), "store, moved\n").expect("write");
    git(
        &home,
        &store,
        &["commit", "-q", "-am", "here: one, t3code, readme"],
    );
    newgit(&home, &store, &["merge", "main"]);

    let (status, answer) = deliver(&server, "push", "d-1", &push_event(&merged, "jack"));
    assert_eq!(status, 202, "{answer}");
    // The same delivery again, while it waits, is not queued twice.
    let (again, _) = deliver(&server, "push", "d-1", &push_event(&merged, "jack"));
    assert!(again == 200 || again == 202, "{again}");
    let done = pulls_done(&server, &token);
    let ended = last_ended(&done);
    assert_eq!(ended["outcome"], "landed", "{done}");

    let a = main_file(&server, &token, &temp, "crates/serve/a.rs");
    assert!(
        a.contains("fn one() { 1 }"),
        "main's own change was lost: {a}"
    );
    assert!(
        a.contains("fn five() { 5 }"),
        "the mirror's change did not land: {a}"
    );
    assert_eq!(
        main_file(&server, &token, &temp, "apps/t3code/app.ts"),
        "export const t3 = 1\n"
    );

    // Then the export: a fast-forward from the merged pull request, which
    // stays on the mirror as itself.
    let tip = mirror_main_until(&mirror, |tip| {
        tip != merged && ancestor(&mirror, &merged, tip)
    });
    assert!(ancestor(&mirror, &exported, &tip));
    let files = git(&home, &mirror, &["ls-tree", "-r", "--name-only", &tip]);
    assert!(!files.contains("apps/t3code"), "{files}");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = state(&server, &token);
        if now["exports"][0]["state"] == "up to date" {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "the export never caught up: {now}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[test]
fn a_webhook_is_refused_unsigned_or_from_an_undeclared_repository() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let _mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    login(&server, &home, "jack");
    let _store = mirrored_project(&temp, &home, &server);

    let body = push_event(&"a".repeat(40), "jack");
    let (status, _) = http(
        &server.url,
        "POST",
        "/v1/webhooks/github?project=jack/store",
        &[("x-github-event", "push")],
        &body,
    );
    assert_eq!(status, 401, "unsigned");
    let wrong = newgit_serve::mirror::webhook::sign("not the secret", body.as_bytes());
    let (status, _) = http(
        &server.url,
        "POST",
        "/v1/webhooks/github?project=jack/store",
        &[
            ("x-github-event", "push"),
            ("x-hub-signature-256", wrong.as_str()),
        ],
        &body,
    );
    assert_eq!(status, 401, "signed with another secret");
    let (status, answer) = deliver(&server, "ping", "d-ping", "{}");
    assert_eq!(status, 200, "{answer}");
    let other = body.replace("jack/mirror", "jack/elsewhere");
    let (status, answer) = deliver(&server, "push", "d-other", &other);
    assert_eq!(status, 403, "{answer}");
    let stranger = push_event(&"a".repeat(40), "mallory");
    let (status, answer) = deliver(&server, "push", "d-mallory", &stranger);
    assert_eq!(status, 403, "{answer}");
    // A project this server has no secret for.
    let (status, _) = http(
        &server.url,
        "POST",
        "/v1/webhooks/github?project=jack/other",
        &[("x-github-event", "ping")],
        "{}",
    );
    assert_eq!(status, 503);
}
