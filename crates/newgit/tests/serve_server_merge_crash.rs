// `newgit serve` killed between writing the server's merge commit and
// writing its landing down (#411). The merge commit was placed in root's
// store first, as any merge's commits are, and the log never named it:
//
// - after the restart, nothing landed: main is what it was, the merge commit
//   is on no branch, and it was never in a store a wider audience reads, so
//   a non-member cannot fetch it by id;
// - the merger's finish is told to start the merge again (merges in flight
//   are kept in memory), and merging again lands, merged by the server.

include!("support/serve.rs");
include!("support/serve_merge.rs");

#[test]
fn killed_between_the_merge_commit_and_the_log_entry_nothing_landed() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let mut server = start_with_faults(&temp, &faults);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let eve = login(&server, &temp.join("eve"), "eve");
    let _store = merge_project(&temp, &home, &server);
    let project = server.data.join("homes/jack/store");
    let devs = project.join("stores/project-devs.git");
    let public = project.join("stores/public.git");
    let base = list_of(&server, &token, "main");

    let root_a = clone_tracker(&server, &token, &temp, "root", "root-a");
    let a_root = commit_on(
        &home,
        &root_a,
        &base["root"],
        &[("README.md", Some("one\ntwo\nthree\nfour\n"))],
        &[],
        "readme grows",
    );
    let mut a_list = base.clone();
    a_list.insert("root".into(), a_root.clone());
    let (status, answer) = merge_by_hand(
        &server,
        &token,
        "main",
        &base,
        &a_list,
        &[Sent {
            tracker: "root",
            dir: &root_a,
            commit: a_root.clone(),
        }],
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");

    let root_b = clone_tracker(&server, &token, &temp, "root", "root-b");
    let b_root = commit_on(
        &home,
        &root_b,
        &base["root"],
        &[("docs/more.md", Some("more\n"))],
        &[],
        "more docs",
    );
    let mut b_list = base.clone();
    b_list.insert("root".into(), b_root.clone());
    let sent = [Sent {
        tracker: "root",
        dir: &root_b,
        commit: b_root.clone(),
    }];
    let (status, begun) = begin_by_hand(&server, &token, "main", &base, &b_list, &sent);
    assert_eq!(status, 200, "{begun}");
    std::fs::write(&faults, "before-log-commit\n").expect("faults");
    let (status, _) = finish_by_hand(
        &server,
        &token,
        "main",
        &base,
        &b_list,
        serde_json::json!([]),
    );
    assert_ne!(
        status, 200,
        "the server stopped before writing the landing down"
    );
    std::fs::write(&faults, "").expect("faults");
    drop(server);
    server = start_with_faults(&temp, &faults);

    assert_eq!(
        list_of(&server, &token, "main"),
        a_list,
        "nothing of B landed"
    );
    // The merge commit the server wrote: placed in root's store, at a
    // landing ref, on no branch.
    let waiting = in_store(
        &home,
        &devs,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/namespaces/t/refs/namespaces/root/refs/newgit/landing/",
        ],
    );
    let merge = waiting
        .lines()
        .find(|commit| parents_of(&home, &devs, commit) == vec![a_root.clone(), b_root.clone()])
        .unwrap_or_else(|| panic!("the server's merge commit was placed: {waiting}"))
        .to_owned();
    let main = in_store(
        &home,
        &devs,
        &[
            "rev-parse",
            "refs/namespaces/t/refs/namespaces/root/refs/heads/main",
        ],
    );
    assert_eq!(main, a_root);
    assert!(!store_has(&public, &merge));
    let reader = temp.join("eve-repo");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    for name in ["blog", "root"] {
        let url = format!("{}/_git/jack.store.t.{name}.git", server.url);
        let by_id = git_as(&eve, &reader, &["fetch", "-q", &url, &merge]);
        assert!(
            !by_id.status.success(),
            "fetched by id through {name}'s URL"
        );
    }

    // Finished again: forgotten, so started again, and then it lands.
    let (status, answer) = finish_by_hand(
        &server,
        &token,
        "main",
        &base,
        &b_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 400, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|error| error.contains("start the merge again")),
        "{answer}"
    );
    let (status, answer) = merge_by_hand(
        &server,
        &token,
        "main",
        &base,
        &b_list,
        &sent,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["merged"], true);
    let landed = list_of(&server, &token, "main");
    assert_eq!(
        parents_of(&home, &devs, &landed["root"]),
        vec![a_root, b_root]
    );
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "docs/more.md").as_deref(),
        Some("more\n")
    );
}
