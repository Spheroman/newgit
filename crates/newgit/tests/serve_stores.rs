// `newgit serve` keeps one object store per audience, each tracker a Git
// namespace in it (#409). A store is never a boundary within itself — under
// protocol v2 any object in it can be fetched by id — so what this proves is
// that no store holds two audiences: a public tracker's readers cannot fetch
// a members-only commit through the public tracker's URL, because it is not
// in that store at all. And when a definition changes a tracker's audience,
// its history moves to the new audience's store with the landing.

include!("support/serve.rs");

fn stores_of(server: &Server) -> Utf8PathBuf {
    server.data.join("homes/jack/store/stores")
}

/// Every ref in a store, by name.
fn refs_in(store: &Utf8Path) -> Vec<String> {
    let out = git(
        store,
        store,
        &[
            "--git-dir",
            store.as_str(),
            "for-each-ref",
            "--format=%(refname)",
        ],
    );
    out.lines().map(str::to_owned).collect()
}

#[test]
fn each_audience_has_a_store_of_its_own() {
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

    // root and access are the members': one store, refs in reftable, made for
    // that audience and no other. Their seeds are gone once they joined `main`.
    let stores = stores_of(&server);
    let devs = stores.join("project-devs.git");
    assert_eq!(
        git(
            &home,
            &temp,
            &["--git-dir", devs.as_str(), "rev-parse", "--show-ref-format"]
        ),
        "reftable"
    );
    assert_eq!(
        git(
            &home,
            &temp,
            &["--git-dir", devs.as_str(), "config", "newgit.audience"]
        ),
        "project-devs"
    );
    assert!(!stores.join("tracker").exists(), "a seed is left alone");
    let in_devs = refs_in(&devs);
    assert!(
        in_devs.contains(&"refs/namespaces/t/refs/namespaces/root/refs/heads/main".to_owned()),
        "{in_devs:?}"
    );
    assert!(
        in_devs.contains(&"refs/namespaces/t/refs/namespaces/access/refs/heads/main".to_owned()),
        "{in_devs:?}"
    );

    // A public tracker joins `main`, and its history goes to the public store.
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
    let public = stores.join("public.git");
    let in_public = refs_in(&public);
    assert!(
        !in_public.is_empty()
            && in_public
                .iter()
                .all(|name| name.starts_with("refs/namespaces/t/refs/namespaces/site/")),
        "{in_public:?}"
    );
    assert!(
        !refs_in(&devs).iter().any(|name| name.contains("/site/")),
        "nothing of site's is beside root's"
    );

    // Anyone reads it; nobody reaches root through it, even by id.
    let site = format!("{}/_git/jack.store.t.site.git", server.url);
    let root = format!("{}/_git/jack.store.t.root.git", server.url);
    let listed = git_as(&eve, &temp, &["ls-remote", &site]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("refs/heads/main"),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(!git_as(&eve, &temp, &["ls-remote", &root]).status.success());
    let root_main = git(&home, &store, &["rev-parse", "HEAD"]);
    let reader = temp.join("eve-repo");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    let by_id = git_as(&eve, &reader, &["fetch", "-q", &site, &root_main]);
    assert!(
        !by_id.status.success(),
        "a members-only commit was fetched by id through a public tracker"
    );

    // Its definition narrows its audience, and the landing moves it.
    let definition = store.join(".newgit/trackers/site.toml");
    let text = std::fs::read_to_string(&definition).expect("site.toml");
    std::fs::write(
        &definition,
        text.replace("audience = \"public\"", "audience = \"project-devs\""),
    )
    .expect("write site.toml");
    git(&home, &store, &["commit", "-q", "-am", "site is ours"]);
    newgit(&home, &store, &["merge", "main"]);
    assert!(
        refs_in(&devs)
            .iter()
            .any(|name| name == "refs/namespaces/t/refs/namespaces/site/refs/heads/main"),
        "{:?}",
        refs_in(&devs)
    );
    assert!(refs_in(&public).is_empty(), "{:?}", refs_in(&public));
    assert!(!git_as(&eve, &temp, &["ls-remote", &site]).status.success());
    let mine = git_as(&token, &temp, &["ls-remote", &site]);
    assert!(
        String::from_utf8_lossy(&mine.stdout).contains("refs/heads/main"),
        "{}",
        String::from_utf8_lossy(&mine.stderr)
    );
}
