// A definition whose mount the client refuses to load never lands on `main`
// (#450): serve holds the mount to the client's own rule
// (`newgit_core::tracker::mount_problem`) and answers with the client's own
// message, so the two never disagree about where a tracker sits.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

/// What the client says loading `blog.toml` with `mount = "<mount>"`.
fn client_refusal(mount: &str) -> String {
    let text = format!(
        "audience = \"public\"\nstorage = \"local\"\ngit = true\nmount = \"{mount}\"\npaths = [\".\"]\n"
    );
    newgit_core::tracker::TrackerDefinition::from_text(
        "blog",
        camino::Utf8Path::new(".newgit/trackers/blog.toml"),
        &text,
    )
    .expect_err("the client refuses it")
    .to_string()
}

#[test]
fn a_mount_the_client_refuses_is_refused_at_landing_with_its_message() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, _ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");

    for (n, mount) in ["/blog", "../blog", "blog/../..", ""].iter().enumerate() {
        let change = blog_definition(
            &server,
            &jack,
            &temp,
            &home,
            &base,
            &[("mount", mount)],
            &format!("refused-{n}"),
        );
        let (status, answer) = merge_by_hand(
            &server,
            &jack,
            "main",
            &base,
            &change.list,
            &change.sent(),
            change.moves(&base),
        );
        assert_eq!(status, 400, "`{mount}`: {answer}");
        assert_eq!(
            answer["error"].as_str(),
            Some(client_refusal(mount).as_str()),
            "`{mount}`: the client's own message"
        );
        assert_eq!(
            list_of(&server, &jack, "main"),
            base,
            "`{mount}`: nothing landed"
        );
    }

    // What the client normalizes, serve does too: `./blog/` and `blog//`
    // are `blog`, and land.
    let change = blog_definition(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("mount", "./blog//")],
        "normalized",
    );
    let (status, answer) = merge_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &change.list,
        &change.sent(),
        change.moves(&base),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(list_of(&server, &jack, "main"), change.list);
}
