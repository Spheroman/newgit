// The rules a server merge lands under are read under the recheck's lock
// (#411 review, items 6 and 9): a member removed while the merge ran does
// not land it, and a change to the definitions on a branch other than `main`
// (where definitions are not read from) moves no epoch.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

/// ana's change to root (members maintain it) merges, then waits before its
/// recheck; jack removes her meanwhile. Her recheck finds her membership
/// changed and starts again, and the rules read again refuse her.
#[test]
fn a_member_removed_while_the_merge_waits_does_not_land_it() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let (home, jack, ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[("ana.md", Some("ana\n"))],
        "ana",
    );
    std::fs::write(&faults, "delay-merge-by-ana\n").expect("faults");
    let finishing = {
        let (url, ana, base, list) = (
            server.url.clone(),
            ana.clone(),
            base.clone(),
            ana_list.clone(),
        );
        std::thread::spawn(move || {
            let bearer = format!("Bearer {ana}");
            http(
                &url,
                "PUT",
                "/v1/projects/jack/store/branches/main",
                &[("authorization", bearer.as_str())],
                &serde_json::json!({ "base": base, "list": list, "moves": [] }).to_string(),
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(400));
    remove_member(&server, &jack, "ana");
    let (status, answer) = finishing.join().expect("joined");
    assert_eq!(status, 403, "{answer}");
    assert_eq!(list_of(&server, &jack, "main"), base, "nothing landed");
}

/// A change to blog's definition merged into `dev` lands without a barrier
/// and leaves the epoch where it was; the same change on `main` moves it.
#[test]
fn a_definitions_change_off_main_moves_no_epoch() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, _ana) = two_people(&server, &temp);
    let main = list_of(&server, &jack, "main");
    let bearer = format!("Bearer {jack}");
    let (status, answer) = http(
        &server.url,
        "PUT",
        "/v1/projects/jack/store/branches/dev",
        &[("authorization", bearer.as_str())],
        &serde_json::json!({ "list": main }).to_string(),
    );
    assert!(status == 200 || status == 201, "{status} {answer}");
    let before = epoch(&server, &jack);

    let change = blog_definition(
        &server,
        &jack,
        &temp,
        &home,
        &main,
        &[("maintainers", "user:ana")],
        "dev",
    );
    let (status, answer) = merge_by_hand(
        &server,
        &jack,
        "dev",
        &main,
        &change.list,
        &change.sent(),
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(list_of(&server, &jack, "dev"), change.list);
    assert_eq!(
        epoch(&server, &jack),
        before,
        "dev's definitions are not read"
    );

    set_blog(
        &server,
        &jack,
        &temp,
        &home,
        &[("maintainers", "user:ana")],
        "main",
    );
    assert_eq!(epoch(&server, &jack), before + 1, "main's are");
}
