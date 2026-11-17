// Tags made on a mirror, brought in by `newgit serve`'s webhook (#453). A
// tag lands nothing: the root commit on `main` whose view is the commit it
// names is found, and the tag is made there as one pushed from a project
// root is, never moved.
//
// - A lightweight and an annotated tag made on the mirror are made here,
//   the annotated one with its tagger and message; readers see both. A pull
//   request merged and then tagged is tagged once it has landed: the tag's
//   pull waits behind the branch's.
// - A tag naming a commit that is not on `main` is refused, saying so, and
//   made once the commit has landed and the event is sent again.
// - A tag that would move is refused, and stays where it was.
// - A tag pushed from the project root comes back from the mirror as an
//   event, and ends `nothing`: it is there already.

include!("support/serve.rs");
include!("support/serve_mirror.rs");

/// GitHub's push event for tag `name` on `jack/mirror` now at `object`.
fn tag_event(name: &str, object: &str, pusher: &str) -> String {
    serde_json::json!({
        "ref": format!("refs/tags/{name}"),
        "after": object,
        "deleted": false,
        "repository": { "full_name": "jack/mirror", "private": false },
        "pusher": { "name": pusher },
    })
    .to_string()
}

fn api(server: &Server, token: &str, path: &str) -> (u16, Value) {
    let bearer = format!("Bearer {token}");
    http(
        &server.url,
        "GET",
        &format!("/v1/projects/jack/store{path}"),
        &[("authorization", bearer.as_str())],
        "",
    )
}

/// Root's commit on `main`.
fn main_root(server: &Server, token: &str) -> String {
    let (status, answer) = api(server, token, "/branches");
    assert_eq!(status, 200, "{answer}");
    answer["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|branch| branch["name"] == "main")
        .and_then(|branch| branch["list"]["root"].as_str())
        .unwrap_or_else(|| panic!("no root on main: {answer}"))
        .to_owned()
}

/// Tag `name` as the remote has it, or `None`.
fn tag(server: &Server, token: &str, name: &str) -> Option<Value> {
    let (status, answer) = api(server, token, &format!("/tags/{name}"));
    (status == 200).then_some(answer)
}

/// How tag `name`'s pull ended, by `/state`.
fn tag_ended(state: &Value, name: &str) -> Value {
    state["ended"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|one| one["tag"] == name)
        .cloned()
        .unwrap_or(Value::Null)
}

#[test]
fn tags_made_on_the_mirror_are_made_here() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let _store = mirrored_project(&temp, &home, &server);
    let exported = mirror_main_until(&mirror, |tip| !tip.is_empty());
    let root = main_root(&server, &token);

    // A lightweight tag and an annotated one, made on the mirror.
    let work = mirror_clone(&home, &temp, &mirror, "work");
    git(&home, &work, &["tag", "v1"]);
    git(&home, &work, &["tag", "-a", "v2", "-m", "second release"]);
    git(&home, &work, &["push", "-q", "origin", "v1", "v2"]);
    let annotated = git(&home, &work, &["rev-parse", "refs/tags/v2"]);
    assert_ne!(
        annotated, exported,
        "an annotated tag is an object of its own"
    );
    let (status, answer) = deliver(&server, "push", "d-v1", &tag_event("v1", &exported, "jack"));
    assert_eq!(status, 202, "{answer}");
    let (status, answer) = deliver(
        &server,
        "push",
        "d-v2",
        &tag_event("v2", &annotated, "jack"),
    );
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(tag_ended(&done, "v1")["outcome"], "tagged", "{done}");
    assert_eq!(tag_ended(&done, "v2")["outcome"], "tagged", "{done}");

    let v1 = tag(&server, &token, "v1").expect("v1 is made");
    assert_eq!(v1["list"]["root"], root.as_str(), "{v1}");
    assert_eq!(v1["annotation"], Value::Null, "{v1}");
    let v2 = tag(&server, &token, "v2").expect("v2 is made");
    assert_eq!(v2["list"]["root"], root.as_str(), "{v2}");
    assert_eq!(v2["annotation"]["message"], "second release\n", "{v2}");
    assert!(
        v2["annotation"]["tagger"]
            .as_str()
            .is_some_and(|tagger| tagger.starts_with("test <test@example.com> ")),
        "{v2}"
    );
    // Readers see them as tags.
    let url = format!("{}/jack/store.git", server.url);
    let listed = git_as(&token, &temp, &["ls-remote", &url, "refs/tags/*"]);
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(listed.contains("refs/tags/v1"), "{listed}");
    assert!(listed.contains("refs/tags/v2"), "{listed}");

    // A pull request merged there and tagged at once: both events arrive,
    // and the tag is looked for only after its commit has landed.
    git(&home, &work, &["checkout", "-q", "-b", "fix"]);
    std::fs::write(work.join("crates/serve/b.rs"), "fn b() { 3 }\n").expect("write");
    git(&home, &work, &["commit", "-q", "-am", "b returns three"]);
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
    git(&home, &work, &["tag", "v3"]);
    git(&home, &work, &["push", "-q", "origin", "main", "v3"]);
    let merged = git(&home, &work, &["rev-parse", "HEAD"]);
    let (status, answer) = deliver(&server, "push", "d-merge", &push_event(&merged, "jack"));
    assert_eq!(status, 202, "{answer}");
    let (status, answer) = deliver(&server, "push", "d-v3", &tag_event("v3", &merged, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(tag_ended(&done, "v3")["outcome"], "tagged", "{done}");
    let landed = main_root(&server, &token);
    assert_ne!(landed, root, "the pull request landed");
    let v3 = tag(&server, &token, "v3").expect("v3 is made");
    assert_eq!(v3["list"]["root"], landed.as_str(), "{v3}");
    // The first tags stay where they were made.
    assert_eq!(
        tag(&server, &token, "v1").expect("v1")["list"]["root"],
        root.as_str()
    );
}

#[test]
fn a_tag_naming_a_commit_not_on_main_is_refused_until_it_lands() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());
    // `newgit status` says what the remote says only beside an instance.
    newgit(&home, &store, &["spawn", "idle"]);

    // A commit pushed to the mirror and tagged; only the tag's event comes.
    let work = mirror_clone(&home, &temp, &mirror, "work");
    std::fs::write(work.join("README.md"), "store, released\n").expect("write");
    git(&home, &work, &["commit", "-q", "-am", "the release"]);
    git(&home, &work, &["tag", "v1"]);
    git(&home, &work, &["push", "-q", "origin", "main", "v1"]);
    let pushed = git(&home, &work, &["rev-parse", "HEAD"]);
    let event = tag_event("v1", &pushed, "jack");
    let (status, answer) = deliver(&server, "push", "d-v1", &event);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    let ended = tag_ended(&done, "v1");
    assert_eq!(ended["outcome"], "refused", "{done}");
    let why = ended["why"].as_str().unwrap_or_default();
    assert!(why.contains("tag `v1`"), "{why}");
    assert!(why.contains("not on `main`"), "{why}");
    assert!(tag(&server, &token, "v1").is_none(), "nothing was made");
    let status = newgit(&home, &store, &["status"]);
    assert!(status.contains("was refused"), "{status}");
    assert!(status.contains("tag `v1`"), "{status}");

    // Who may not merge here may not tag here either.
    let (status, answer) = deliver(
        &server,
        "push",
        "d-mallory",
        &tag_event("v9", &pushed, "mallory"),
    );
    assert_eq!(status, 403, "{answer}");
    // And a tag deleted on the mirror deletes nothing here.
    let deleted = serde_json::json!({
        "ref": "refs/tags/v1", "after": "0".repeat(40), "deleted": true,
        "repository": { "full_name": "jack/mirror" }, "pusher": { "name": "jack" },
    })
    .to_string();
    let (status, answer) = deliver(&server, "push", "d-deleted", &deleted);
    assert_eq!(status, 200, "{answer}");
    assert!(answer["ignored"].is_string(), "{answer}");

    // The commit's own event lands it. That does not make the tag, and the
    // tag's refusal stays said until its event is sent again.
    let (status, answer) = deliver(&server, "push", "d-main", &push_event(&pushed, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(tag_ended(&done, "v1")["outcome"], "refused", "{done}");
    assert!(tag(&server, &token, "v1").is_none());
    let (status, answer) = deliver(&server, "push", "d-v1", &event);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(tag_ended(&done, "v1")["outcome"], "tagged", "{done}");
    let v1 = tag(&server, &token, "v1").expect("v1 is made");
    assert_eq!(
        v1["list"]["root"],
        main_root(&server, &token).as_str(),
        "{v1}"
    );
    let status = newgit(&home, &store, &["status"]);
    assert!(!status.contains("was refused"), "{status}");
}

#[test]
fn a_tag_that_would_move_is_refused() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    let first = mirror_main_until(&mirror, |tip| !tip.is_empty());
    let root = main_root(&server, &token);

    let work = mirror_clone(&home, &temp, &mirror, "work");
    git(&home, &work, &["tag", "v1"]);
    git(&home, &work, &["push", "-q", "origin", "v1"]);
    let (status, answer) = deliver(&server, "push", "d-v1", &tag_event("v1", &first, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(tag_ended(&done, "v1")["outcome"], "tagged", "{done}");

    // `main` moves on, and someone moves the tag on the mirror with it.
    std::fs::write(store.join("README.md"), "store, moved\n").expect("write");
    git(&home, &store, &["commit", "-q", "-am", "here: readme"]);
    newgit(&home, &store, &["merge", "main"]);
    let second = mirror_main_until(&mirror, |tip| tip != first);
    git(&home, &work, &["fetch", "-q", "origin"]);
    git(&home, &work, &["tag", "-f", "v1", &second]);
    git(&home, &work, &["push", "-q", "-f", "origin", "v1"]);
    let (status, answer) = deliver(
        &server,
        "push",
        "d-v1-moved",
        &tag_event("v1", &second, "jack"),
    );
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    let ended = tag_ended(&done, "v1");
    assert_eq!(ended["outcome"], "refused", "{done}");
    let why = ended["why"].as_str().unwrap_or_default();
    assert!(why.contains("tag `v1`"), "{why}");
    assert!(why.contains("never moves"), "{why}");
    assert_eq!(
        tag(&server, &token, "v1").expect("v1")["list"]["root"],
        root.as_str(),
        "it stays where it was made"
    );
    assert_ne!(main_root(&server, &token), root);
}

#[test]
fn a_tag_pushed_from_the_project_root_comes_back_as_nothing() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    // Tagged in the project root and pushed: made on the remote, then on
    // the mirror, by the client. GitHub sends the mirror's new tags back.
    git(&home, &store, &["tag", "v1"]);
    git(&home, &store, &["tag", "-a", "v2", "-m", "from here"]);
    git(&home, &store, &["push", "-q", "origin", "v1", "v2"]);
    let made = tag(&server, &token, "v2").expect("the push made v2 on the remote");
    assert_eq!(made["annotation"]["message"], "from here\n", "{made}");
    for name in ["v1", "v2"] {
        let object = git(&home, &mirror, &["rev-parse", &format!("refs/tags/{name}")]);
        let (status, answer) = deliver(
            &server,
            "push",
            &format!("d-{name}"),
            &tag_event(name, &object, "jack"),
        );
        assert_eq!(status, 202, "{answer}");
    }
    let done = pulls_done(&server, &token);
    assert_eq!(tag_ended(&done, "v1")["outcome"], "nothing", "{done}");
    assert_eq!(tag_ended(&done, "v2")["outcome"], "nothing", "{done}");
    assert_eq!(tag(&server, &token, "v2").expect("v2"), made, "untouched");
    let status = newgit(&home, &store, &["status"]);
    assert!(!status.contains("was refused"), "{status}");
}
