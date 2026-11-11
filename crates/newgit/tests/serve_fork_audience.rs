// Every repo `newgit serve` hands out is served only from the store its
// tracker's current facts call for (#409's review): a fork no less than its
// tracker. `site` is the members' with a fork from instance `feat`; a landing
// widens it to `public`, and the move fails, because a `public.git` is in the
// way that was made for another audience. Nothing is served from the wrong
// store meanwhile: a non-member reads neither `site` nor its fork, and cannot
// fetch root's members-only commit by id through either. Once the store in the
// way is gone, landing again moves `site`, and its fork with it.

include!("support/serve.rs");

fn project(temp: &Utf8Path, home: &Utf8Path, server: &Server) -> Utf8PathBuf {
    let store = temp.join("store");
    std::fs::create_dir_all(store.join("site")).expect("mkdir");
    git(home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "members only\n").expect("write");
    std::fs::write(store.join("site/index.html"), "<h1>hi</h1>\n").expect("write");
    git(home, &store, &["add", "."]);
    git(home, &store, &["commit", "-q", "-m", "initial"]);
    newgit(home, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    git(home, &store, &["add", "-A"]);
    git(home, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(home, &store, &["remote", "add", "--url", &server.url]);
    git(home, &store, &["commit", "-q", "-am", "link"]);
    newgit(
        home,
        &store,
        &[
            "tracker",
            "create",
            "site",
            "--git",
            "--audience",
            "project-devs",
            "--path",
            "site",
        ],
    );
    newgit(home, &store, &["merge", "main"]);
    store
}

#[test]
fn a_fork_is_never_served_from_a_store_its_audience_does_not_call_for() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let eve = login(&server, &temp.join("eve"), "eve");
    let store = project(&temp, &home, &server);
    let stores = server.data.join("homes/jack/store/stores");
    let root_main = git(
        &home,
        &temp,
        &[
            "--git-dir",
            stores.join("project-devs.git").as_str(),
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

    // A store made for another audience sits where `public.git` goes.
    let public = stores.join("public.git");
    git(
        &home,
        &temp,
        &[
            "init",
            "-q",
            "--bare",
            "--ref-format=reftable",
            public.as_str(),
        ],
    );
    git(
        &home,
        &temp,
        &[
            "--git-dir",
            public.as_str(),
            "config",
            "newgit.audience",
            "project-devs",
        ],
    );

    // A landing widens `site`; its move fails, so the landing does not
    // finish.
    let definition = store.join(".newgit/trackers/site.toml");
    let text = std::fs::read_to_string(&definition).expect("site.toml");
    std::fs::write(
        &definition,
        text.replace("audience = \"project-devs\"", "audience = \"public\""),
    )
    .expect("write site.toml");
    git(&home, &store, &["commit", "-q", "-am", "site is public"]);
    let merged = run(
        env!("CARGO_BIN_EXE_newgit"),
        &home,
        &store,
        &["merge", "main"],
    );
    assert!(
        !merged.status.success(),
        "a landing whose objects have nowhere safe to go finished"
    );

    let site = format!("{}/_git/jack.store.t.site.git", server.url);
    let fork = format!("{}/_git/jack.store.i.feat.site.git", server.url);
    let reader = temp.join("eve-repo");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    for url in [&site, &fork] {
        let listed = git_as(&eve, &reader, &["ls-remote", url]);
        assert!(!listed.status.success(), "{url} was served to a non-member");
        let by_id = git_as(&eve, &reader, &["fetch", "-q", url, &root_main]);
        assert!(
            !by_id.status.success(),
            "root's commit was fetched by id through {url}"
        );
    }
    assert!(
        git(
            &home,
            &temp,
            &["--git-dir", public.as_str(), "for-each-ref"]
        )
        .is_empty(),
        "nothing went into the store made for another audience"
    );

    // The store in the way gone, landing again moves `site` and its fork.
    std::fs::remove_dir_all(&public).expect("remove");
    newgit(&home, &store, &["merge", "main"]);
    assert_eq!(
        git(
            &home,
            &temp,
            &["--git-dir", public.as_str(), "config", "newgit.audience"]
        ),
        "public"
    );
    for url in [&site, &fork] {
        let listed = git_as(&eve, &reader, &["ls-remote", url]);
        assert!(
            String::from_utf8_lossy(&listed.stdout).contains("refs/heads/main"),
            "{url}: {}",
            String::from_utf8_lossy(&listed.stderr)
        );
        let by_id = git_as(&eve, &reader, &["fetch", "-q", url, &root_main]);
        assert!(
            !by_id.status.success(),
            "root's commit was fetched by id through {url}"
        );
    }
}
