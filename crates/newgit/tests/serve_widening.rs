// A widening landing brings its commits into the tracker's old, narrower
// store before it moves, so one that fails leaves the tracker where it was
// (#409's review). `site` is the members', `blog` is public. One `newgit
// merge` makes `site` public and adds a file whose content is root's
// members-only `README.md`; a `git` wrapper leaves that blob out of the
// uploaded pack, so it resolves only against the members' store.
//
// The first time, the members' store cannot take objects, so bringing `site`'s
// commits in fails at `finish`: the landing does not land, `site` is still in
// the members' store with its row unchanged, and no public URL serves its
// members-only commit or blob by id. Merged again with the store fixed (the
// pack still thin), the widening lands: its commits resolve in the store they
// are brought into, then `site` moves to `public.git`. Moving first, as
// before, failed here, with `site`'s members-only history left in
// `public.git`.

include!("support/serve.rs");

fn project(temp: &Utf8Path, home: &Utf8Path, server: &Server) -> Utf8PathBuf {
    let store = temp.join("store");
    std::fs::create_dir_all(store.join("site")).expect("mkdir");
    std::fs::create_dir_all(store.join("blog")).expect("mkdir");
    git(home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "members only\n").expect("write");
    std::fs::write(store.join("site/index.html"), "<h1>members site</h1>\n").expect("write");
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
    for (tracker, audience) in [("site", "project-devs"), ("blog", "public")] {
        newgit(
            home,
            &store,
            &[
                "tracker",
                "create",
                tracker,
                "--git",
                "--audience",
                audience,
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

fn row(data: &Utf8Path, tracker: &str) -> String {
    let db = rusqlite::Connection::open(data.join("server.sqlite").as_std_path()).expect("db");
    db.query_row(
        "SELECT store FROM tracker_stores WHERE home = 'jack/store' AND tracker = ?",
        [tracker],
        |row| row.get(0),
    )
    .expect("site's row")
}

#[test]
fn a_widening_whose_objects_cannot_land_leaves_its_tracker_where_it_was() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    login(&server, &home, "jack");
    let eve = login(&server, &temp.join("eve"), "eve");
    let store = project(&temp, &home, &server);
    let stores = server.data.join("homes/jack/store/stores");
    let public = stores.join("public.git");
    let devs = stores.join("project-devs.git");
    let site_ref = "refs/namespaces/t/refs/namespaces/site/refs/heads/main";
    let members_site = git(
        &home,
        &temp,
        &["--git-dir", devs.as_str(), "rev-parse", site_ref],
    );
    let members_blob = git(&home, &store, &["hash-object", "site/index.html"]);
    assert_eq!(row(&server.data, "site"), "project-devs");
    assert!(!has_object(&public, &members_site));

    // One merge widens `site` and adds a copy of root's members-only README,
    // whose blob the pack leaves out.
    let definition = store.join(".newgit/trackers/site.toml");
    let text = std::fs::read_to_string(&definition).expect("site.toml");
    std::fs::write(
        &definition,
        text.replace("audience = \"project-devs\"", "audience = \"public\""),
    )
    .expect("write site.toml");
    git(&home, &store, &["commit", "-q", "-am", "site is public"]);
    std::fs::write(store.join("site/copy.txt"), "members only\n").expect("write");
    let thin = git(&home, &store, &["hash-object", "README.md"]);
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "a copy"]);
    let bin = temp.join("wrapbin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let real = String::from_utf8(
        Command::new("which")
            .arg("git")
            .output()
            .expect("which")
            .stdout,
    )
    .expect("utf-8");
    let real = real.trim();
    std::fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\ncase \"$*\" in *pack-objects*) \"{real}\" rev-list --objects --stdin | grep -v {thin} | cut -d' ' -f1 | \"{real}\" pack-objects --stdout -q; exit $?;; esac\nexec \"{real}\" \"$@\"\n"
        ),
    )
    .expect("write wrapper");
    run("chmod", &home, &temp, &["+x", bin.join("git").as_str()]);
    let path = format!("{bin}:{}", std::env::var("PATH").expect("PATH"));
    let merge = || {
        Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(store.as_str())
            .args(["merge", "main"])
            .env("HOME", home.as_str())
            .env("PATH", &path)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@e.com")
            .output()
            .expect("newgit merge")
    };

    // The members' store cannot take objects: the upload (into the merger's
    // quarantine) passes, and bringing `site`'s commits in at `finish` fails.
    let objects = devs.join("objects");
    run("chmod", &home, &temp, &["-R", "a-w", objects.as_str()]);
    let merged = merge();
    run("chmod", &home, &temp, &["-R", "u+w", objects.as_str()]);
    assert!(
        !merged.status.success(),
        "a landing whose objects could not be brought in finished: {}",
        String::from_utf8_lossy(&merged.stdout)
    );
    assert!(
        String::from_utf8_lossy(&merged.stderr).contains("(500)")
            && String::from_utf8_lossy(&merged.stderr)
                .contains("PUT /v1/projects/jack/store/branches/main"),
        "it failed at `finish`, after its upload: {}",
        String::from_utf8_lossy(&merged.stderr)
    );

    // Nothing moved: `site` is the members', where it was.
    assert_eq!(row(&server.data, "site"), "project-devs");
    assert_eq!(
        git(
            &home,
            &temp,
            &["--git-dir", devs.as_str(), "rev-parse", site_ref]
        ),
        members_site
    );
    assert!(
        !git(
            &home,
            &temp,
            &["--git-dir", public.as_str(), "for-each-ref"]
        )
        .contains("namespaces/site/"),
        "site's refs went into public.git"
    );
    for object in [&members_site, &members_blob, &thin] {
        assert!(!has_object(&public, object), "{object} is in public.git");
    }

    // So no public URL serves its members-only commit or blob, by id or
    // otherwise.
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
    assert!(
        !git_as(&eve, &reader, &["ls-remote", &site])
            .status
            .success(),
        "site was served to a non-member"
    );
    for url in [&blog, &site] {
        for object in [&members_site, &members_blob] {
            let by_id = git_as(&eve, &reader, &["fetch", "-q", url, object]);
            assert!(
                !by_id.status.success(),
                "{object} was fetched by id through {url}"
            );
        }
    }

    // The store fixed, the same thin merge lands the widening.
    let merged = merge();
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    assert_eq!(row(&server.data, "site"), "public");
    let landed = git(
        &home,
        &temp,
        &["--git-dir", public.as_str(), "rev-parse", site_ref],
    );
    assert_ne!(landed, members_site);
    let listed = git_as(&eve, &reader, &["ls-remote", &site]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(&landed),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let fetched = git_as(&eve, &reader, &["fetch", "-q", &site, &landed]);
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
}
