// A rooted target on `newgit serve` (#414): `t3code`, exported on its own
// to `jack/t3code` with its own files at the top. The server exports its
// view there; a commit merged there lands in `t3code` and moves its pin in
// root in the same landing; the export then finds the mirror where it is.

include!("support/serve.rs");
include!("support/serve_mirror.rs");

#[test]
fn a_commit_on_a_rooted_targets_mirror_lands_in_its_tracker_and_moves_the_pin() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let t3 = github.repo("jack/t3code");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());
    set_definition_export(
        &store,
        "t3code",
        Some("{ url = \"github.com/jack/t3code\", name = \"t3code\" }"),
    );
    git(
        &home,
        &store,
        &["commit", "-q", "-am", "t3code exports on its own"],
    );
    newgit(&home, &store, &["merge", "main"]);

    let tip = mirror_main_until(&t3, |tip| !tip.is_empty());
    let files = git(&home, &t3, &["ls-tree", "-r", "--name-only", &tip]);
    assert!(
        files.lines().any(|file| file == "app.ts"),
        "its own files at the top: {files}"
    );
    assert!(!files.contains("README.md"), "{files}");

    let work = mirror_clone(&home, &temp, &t3, "t3-work");
    std::fs::write(work.join("app.ts"), "export const upstream = 1\n").expect("write");
    git(&home, &work, &["commit", "-q", "-am", "upstream change"]);
    git(&home, &work, &["push", "-q", "origin", "main"]);
    let pushed = git(&home, &work, &["rev-parse", "HEAD"]);

    let event = serde_json::json!({
        "ref": "refs/heads/main", "after": pushed, "deleted": false,
        "repository": { "full_name": "jack/t3code" }, "pusher": { "name": "jack" },
    })
    .to_string();
    let (status, answer) = deliver(&server, "push", "d-t3", &event);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "landed", "{done}");
    assert_eq!(
        main_file(&server, &token, &temp, "apps/t3code/app.ts"),
        "export const upstream = 1\n"
    );
    // The project's own mirror never sees it.
    let project_tip = mirror_main_until(&mirror, |_| true);
    let files = git(
        &home,
        &mirror,
        &["ls-tree", "-r", "--name-only", &project_tip],
    );
    assert!(!files.contains("apps/t3code"), "{files}");
    // And the rooted mirror is where it was: the commit came back as itself.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = state(&server, &token);
        let exports = now["exports"].as_array().cloned().unwrap_or_default();
        let t3_export = exports
            .iter()
            .find(|export| export["target"] == "t3code")
            .cloned();
        if t3_export.as_ref().is_some_and(|export| {
            export["tip"] == pushed.as_str() && export["state"] == "up to date"
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "t3code's export never came back to {pushed}: {now}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(git(&home, &t3, &["rev-parse", "refs/heads/main"]), pushed);
}
