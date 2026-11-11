// The server's merge composes trees across trackers with different
// audiences, and nothing composed reaches a store (#411). Root is the
// members', `blog` is public. Two merges from one base both move `blog`, and
// one also moves root's README, so the server composes root's private files
// with blog's, merges, and splits.
//
// - The composed view commit (named by root's merge commit's
//   `Newgit-Split` trailer) is in no store, and the scratch repository it
//   was made in is gone.
// - blog's merge commit, in the public store, holds blog's files and a stock
//   message, and nothing of root's.
// - A non-member can fetch blog's merge commit by id through blog's URL,
//   and cannot fetch root's merge commit, the composed view commit, or the
//   private README, by id, the same way.

include!("support/serve.rs");
include!("support/serve_merge.rs");

/// Every bare repository under `dir`.
fn repositories(dir: &Utf8Path, out: &mut Vec<Utf8PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = Utf8PathBuf::from_path_buf(entry.path()).expect("utf8");
        if !path.is_dir() {
            continue;
        }
        if path.extension() == Some("git") && path.join("objects").is_dir() {
            out.push(path);
        } else {
            repositories(&path, out);
        }
    }
}

#[test]
fn nothing_composed_reaches_a_store_and_a_non_member_fetches_nothing_of_it() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let eve = login(&server, &temp.join("eve"), "eve");
    let _store = merge_project(&temp, &home, &server);
    let project = server.data.join("homes/jack/store");
    let devs = project.join("stores/project-devs.git");
    let public = project.join("stores/public.git");
    let base = list_of(&server, &token, "main");
    let secret = "the launch date\n";
    let mut made = Vec::new();
    for (side, file) in [("a", "a.md"), ("b", "b.md")] {
        let blog = clone_tracker(&server, &token, &temp, "blog", &format!("blog-{side}"));
        let root = clone_tracker(&server, &token, &temp, "root", &format!("root-{side}"));
        let blog_commit = commit_on(
            &home,
            &blog,
            &base["blog"],
            &[(file, Some("words\n"))],
            &[],
            &format!("add {file}"),
        );
        let readme: &[(&str, Option<&str>)] = if side == "a" {
            &[("README.md", Some(secret))]
        } else {
            &[]
        };
        let root_commit = commit_on(
            &home,
            &root,
            &base["root"],
            readme,
            &[("blog", &blog_commit)],
            "Update blog",
        );
        let mut list = base.clone();
        list.insert("root".into(), root_commit.clone());
        list.insert("blog".into(), blog_commit.clone());
        let (status, answer) = merge_by_hand(
            &server,
            &token,
            "main",
            &base,
            &list,
            &[
                Sent {
                    tracker: "blog",
                    dir: &blog,
                    commit: blog_commit.clone(),
                },
                Sent {
                    tracker: "root",
                    dir: &root,
                    commit: root_commit,
                },
            ],
            serde_json::json!([]),
        );
        assert_eq!(status, 200, "{answer}");
        made.push(blog_commit);
    }
    let landed = list_of(&server, &token, "main");
    let root_merge = &landed["root"];
    let blog_merge = &landed["blog"];
    assert_eq!(parents_of(&home, &public, blog_merge), made);

    // The view commit the root merge was split from: in no store.
    let message = in_store(&home, &devs, &["log", "-1", "--format=%B", root_merge]);
    let view = message
        .lines()
        .find_map(|line| line.strip_prefix("Newgit-Split: "))
        .expect("root's merge names the view commit it was split from")
        .to_owned();
    let mut all = Vec::new();
    repositories(&project, &mut all);
    for repository in &all {
        assert!(
            !store_has(repository, &view),
            "the composed view commit is in {repository}"
        );
    }
    let scratch = project.join("scratch");
    assert!(
        std::fs::read_dir(&scratch).map_or(true, |mut entries| entries.next().is_none()),
        "a scratch repository was left in {scratch}"
    );

    // blog's merge commit: blog's files, a stock message.
    let files = in_store(
        &home,
        &public,
        &["ls-tree", "-r", "--name-only", blog_merge],
    );
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        vec!["a.md", "b.md", "post.md"]
    );
    assert_eq!(
        in_store(&home, &public, &["log", "-1", "--format=%B", blog_merge]),
        "Merge `jack` into main"
    );
    let secret_blob = {
        let path = temp.join("secret");
        std::fs::write(&path, secret).expect("write");
        git(&home, &temp, &["hash-object", path.as_str()])
    };
    assert!(store_has(&devs, &secret_blob));
    for object in [root_merge.as_str(), view.as_str(), secret_blob.as_str()] {
        assert!(
            !store_has(&public, object),
            "{object} is in the public store"
        );
    }

    // Over Git, as a non-member, through blog's URL.
    let reader = temp.join("eve-repo");
    git(&home, &temp, &["init", "-q", reader.as_str()]);
    let blog_url = format!("{}/_git/jack.store.t.blog.git", server.url);
    let fetched = git_as(&eve, &reader, &["fetch", "-q", &blog_url, blog_merge]);
    assert!(
        fetched.status.success(),
        "blog's merge commit is public: {}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    for object in [root_merge.as_str(), view.as_str(), secret_blob.as_str()] {
        let by_id = git_as(&eve, &reader, &["fetch", "-q", &blog_url, object]);
        assert!(
            !by_id.status.success(),
            "{object} was fetched by id through blog's URL"
        );
    }
}
