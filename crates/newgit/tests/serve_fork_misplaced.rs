// `served` refuses a fork whose tracker is not in the store its definition
// calls for (#409's review). No request makes that state any more (a
// widening's landing brings its commits in before it moves, and a move that
// stops part way is finished by the next `settle`), so it is made here in the
// data directory: `site` is public, with a fork from instance `feat`; its refs
// (the fork's among them) are copied into the members' store with no `HEAD`
// of its own, `public.git` is gone, and its row says `project-devs`. `settle`
// finds nothing to move, so `served` is all that stands between a non-member,
// whom `site`'s definition lets read the fork, and the members' store: the
// fork URL is refused, and root's members-only commit cannot be fetched by id
// through it.

include!("support/serve.rs");

#[test]
fn a_fork_in_a_store_its_tracker_does_not_call_for_is_refused() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let eve = login(&server, &temp.join("eve"), "eve");

    let store = temp.join("store");
    std::fs::create_dir_all(store.join("site")).expect("mkdir");
    git(&home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "members only\n").expect("write");
    std::fs::write(store.join("site/index.html"), "<h1>hi</h1>\n").expect("write");
    git(&home, &store, &["add", "."]);
    git(&home, &store, &["commit", "-q", "-m", "initial"]);
    newgit(&home, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(&home, &store, &["remote", "add", "--url", &server.url]);
    git(&home, &store, &["commit", "-q", "-am", "link"]);
    newgit(
        &home,
        &store,
        &[
            "tracker",
            "create",
            "site",
            "--git",
            "--audience",
            "public",
            "--path",
            "site",
        ],
    );
    newgit(&home, &store, &["merge", "main"]);

    let stores = server.data.join("homes/jack/store/stores");
    let public = stores.join("public.git");
    let devs = stores.join("project-devs.git");
    let root_main = git(
        &home,
        &temp,
        &[
            "--git-dir",
            devs.as_str(),
            "rev-parse",
            "refs/namespaces/t/refs/namespaces/root/refs/heads/main",
        ],
    );

    // Instance `feat`, with a fork of `site`.
    let bearer = format!("Bearer {token}");
    let auth = [("authorization", bearer.as_str())];
    let (status, made) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/instances",
        &auth,
        &format!("{{\"name\":\"feat\",\"base_rev\":\"{root_main}\"}}"),
    );
    assert_eq!(status, 201, "{made}");
    let (status, forked) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/instances/feat/trackers/site/fork",
        &auth,
        "",
    );
    assert_eq!(status, 200, "{forked}");

    let fork = format!("{}/_git/jack.store.i.feat.site.git", server.url);
    let reader = temp.join("eve-repo");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    let listed = git_as(&eve, &reader, &["ls-remote", &fork]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("refs/heads/main"),
        "the fork of a public tracker is anyone's to read: {}",
        String::from_utf8_lossy(&listed.stderr)
    );

    // `site`'s refs, the fork's among them, into the members' store; its
    // public store gone; its row naming the members' store.
    let refs = "refs/namespaces/t/refs/namespaces/site/refs/*";
    git(
        &home,
        &temp,
        &[
            "--git-dir",
            devs.as_str(),
            "fetch",
            "-q",
            public.as_str(),
            &format!("+{refs}:{refs}"),
        ],
    );
    assert!(
        git(&home, &temp, &["--git-dir", devs.as_str(), "for-each-ref"])
            .contains("refs/namespaces/i/refs/namespaces/feat/refs/heads/main"),
        "the fork's refs are in the members' store"
    );
    std::fs::remove_dir_all(&public).expect("remove public.git");
    let db =
        rusqlite::Connection::open(server.data.join("server.sqlite").as_std_path()).expect("db");
    let changed = db
        .execute(
            "UPDATE tracker_stores SET store = 'project-devs' WHERE home = 'jack/store' AND tracker = 'site'",
            [],
        )
        .expect("update site's row");
    assert_eq!(changed, 1);
    drop(db);

    let listed = git_as(&eve, &reader, &["ls-remote", &fork]);
    assert!(
        !listed.status.success(),
        "the fork was served from the members' store: {}",
        String::from_utf8_lossy(&listed.stdout)
    );
    let by_id = git_as(&eve, &reader, &["fetch", "-q", &fork, &root_main]);
    assert!(
        !by_id.status.success(),
        "root's members-only commit was fetched by id through the fork"
    );
}
