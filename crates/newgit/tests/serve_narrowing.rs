// Narrowing a tracker's audience never puts what it narrows into the wider
// store (#409's review). One `newgit merge` both moves `site` from `public` to
// `project-devs` and adds a secret to it. The landing's objects wait in a
// quarantine of the merger's own and go straight to the store the landing's
// definitions call for, so `public.git` never holds them: no URL into it
// (`blog`'s, a public tracker beside `site`) can serve them by id. Content
// `site` had while it was public stays in `public.git`, where it was already
// readable.

include!("support/serve.rs");

fn project(temp: &Utf8Path, home: &Utf8Path, server: &Server) -> Utf8PathBuf {
    let store = temp.join("store");
    std::fs::create_dir_all(store.join("site")).expect("mkdir");
    std::fs::create_dir_all(store.join("blog")).expect("mkdir");
    git(home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "members only\n").expect("write");
    std::fs::write(store.join("site/index.html"), "<h1>hi</h1>\n").expect("write");
    std::fs::write(store.join("blog/post.md"), "hello\n").expect("write");
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
    for tracker in ["site", "blog"] {
        newgit(
            home,
            &store,
            &[
                "tracker",
                "create",
                tracker,
                "--git",
                "--audience",
                "public",
                "--path",
                tracker,
            ],
        );
    }
    newgit(home, &store, &["merge", "main"]);
    store
}

fn has_object(store: &Utf8Path, object: &str) -> bool {
    run(
        "git",
        store,
        store,
        &["--git-dir", store.as_str(), "cat-file", "-e", object],
    )
    .status
    .success()
}

#[test]
fn a_narrowing_landing_never_writes_its_content_into_the_wider_store() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    login(&server, &home, "jack");
    let eve = login(&server, &temp.join("eve"), "eve");
    let store = project(&temp, &home, &server);
    let stores = server.data.join("homes/jack/store/stores");
    let public = stores.join("public.git");
    let devs = stores.join("project-devs.git");
    let site_main = |dir: &Utf8Path| {
        git(
            &home,
            &temp,
            &[
                "--git-dir",
                dir.as_str(),
                "rev-parse",
                "refs/namespaces/t/refs/namespaces/site/refs/heads/main",
            ],
        )
    };
    let was_public = site_main(&public);

    // One merge narrows `site` and lands a secret in it.
    let definition = store.join(".newgit/trackers/site.toml");
    let text = std::fs::read_to_string(&definition).expect("site.toml");
    std::fs::write(
        &definition,
        text.replace("audience = \"public\"", "audience = \"project-devs\""),
    )
    .expect("write site.toml");
    std::fs::write(store.join("site/secret.txt"), "the launch date\n").expect("write");
    git(&home, &store, &["add", "-A"]);
    git(
        &home,
        &store,
        &["commit", "-q", "-m", "site is ours, with a secret"],
    );
    newgit(&home, &store, &["merge", "main"]);

    let narrowed = site_main(&devs);
    assert_ne!(narrowed, was_public, "the landing moved site");
    let secret = git(&home, &store, &["hash-object", "site/secret.txt"]);
    assert!(has_object(&devs, &narrowed) && has_object(&devs, &secret));
    assert!(
        !has_object(&public, &narrowed) && !has_object(&public, &secret),
        "the narrowing landing's objects were written into public.git"
    );

    // So no URL into the public store serves them, by id or otherwise.
    let blog = format!("{}/_git/jack.store.t.blog.git", server.url);
    let site = format!("{}/_git/jack.store.t.site.git", server.url);
    let reader = temp.join("eve-repo");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    assert!(
        git_as(&eve, &reader, &["ls-remote", &blog])
            .status
            .success(),
        "blog is still public"
    );
    for object in [&narrowed, &secret] {
        let by_id = git_as(&eve, &reader, &["fetch", "-q", &blog, object]);
        assert!(
            !by_id.status.success(),
            "{object} was fetched by id through blog's URL"
        );
    }
    assert!(
        !git_as(&eve, &reader, &["ls-remote", &site])
            .status
            .success()
    );

    // What site had while it was public was readable then, and stays where
    // it was (serve.md says so).
    assert!(has_object(&public, &was_public));
}

/// A merge's commits pushed with the password `begin` hands out (the way
/// a pack too large to send ahead lands) go into the merger's landing too,
/// never a store.
#[test]
fn a_landing_push_goes_into_the_mergers_landing_not_the_store() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let store = project(&temp, &home, &server);
    let data = server.data.join("homes/jack/store");
    let devs = data.join("stores/project-devs.git");

    let bearer = format!("Bearer {token}");
    let auth = [("authorization", bearer.as_str())];
    let (status, state) = http(
        &server.url,
        "GET",
        "/v1/projects/jack/store/branches",
        &auth,
        "",
    );
    assert_eq!(status, 200, "{state}");
    let base = state["branches"]
        .as_array()
        .and_then(|branches| branches.iter().find(|branch| branch["name"] == "main"))
        .map(|main| main["list"].clone())
        .expect("main's list");

    // A root commit the remote has not seen.
    let work = temp.join("root-work");
    let root = format!("{}/_git/jack.store.t.root.git", server.url);
    let cloned = git_as(&token, &temp, &["clone", "-q", &root, work.as_str()]);
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    std::fs::write(work.join("secret.txt"), "not yet\n").expect("write");
    git(&home, &work, &["add", "."]);
    git(&home, &work, &["commit", "-q", "-m", "secret"]);
    let commit = git(&home, &work, &["rev-parse", "HEAD"]);

    let mut list = base.clone();
    list["root"] = serde_json::json!(commit);
    let (status, begun) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/branches/main/merge",
        &auth,
        &serde_json::json!({ "base": base, "list": list, "uploaded": false }).to_string(),
    );
    assert_eq!(status, 200, "{begun}");
    let credential = begun["credentials"]
        .as_array()
        .and_then(|all| {
            all.iter()
                .find(|credential| credential["tracker"] == "root")
        })
        .expect("a password for root")
        .clone();
    let remote = credential["remote"].as_str().expect("remote");
    let header = format!(
        "http.extraHeader=Authorization: Bearer {}",
        credential["password"].as_str().expect("password")
    );
    let pushed = run(
        "git",
        &work,
        &work,
        &[
            "-c",
            "credential.helper=",
            "-c",
            &header,
            "push",
            "-q",
            remote,
            &format!("HEAD:refs/newgit/landing/{commit}"),
        ],
    );
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let held = |dir: &Utf8Path| has_object(dir, &commit);
    assert!(held(&data.join("landing/jack/t/root.git")));
    assert!(!held(&devs), "a landing push was written into the store");
    // And nothing but a landing ref may be pushed with it.
    let other = run(
        "git",
        &work,
        &work,
        &[
            "-c",
            "credential.helper=",
            "-c",
            &header,
            "push",
            "-q",
            remote,
            "HEAD:refs/heads/elsewhere",
        ],
    );
    assert!(!other.status.success());
    let _ = store;
}
