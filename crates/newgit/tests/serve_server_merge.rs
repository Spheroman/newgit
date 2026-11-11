// `newgit serve` merges on the server (#411): a merge whose branch moved
// since its base is merged onto what the branch is now, with Git's merge
// over the composed tree, and lands. Nobody is told to merge again except
// for a real conflict, which names the file.
//
// - Two merges begun from one base at different trackers both land. The
//   first fast-forwards to its own commits, ids kept; the second gets one
//   merge commit in root, the head's commit first and the merger's second,
//   and its own `blog` commit as it made it.
// - A tracker both moved gets one merge commit of its own, holding its own
//   files and a stock message, and its `main` follows to it.
// - The same lines of one file changed on both sides is refused, naming it.
// - A file moved between trackers is followed, as in one repository.

include!("support/serve.rs");
include!("support/serve_merge.rs");

fn stores(server: &Server) -> (Utf8PathBuf, Utf8PathBuf) {
    let stores = server.data.join("homes/jack/store/stores");
    (stores.join("project-devs.git"), stores.join("public.git"))
}

fn blog_main(home: &Utf8Path, public: &Utf8Path) -> String {
    in_store(
        home,
        public,
        &[
            "rev-parse",
            "refs/namespaces/t/refs/namespaces/blog/refs/heads/main",
        ],
    )
}

#[test]
fn two_merges_from_one_base_at_different_trackers_both_land() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let _store = merge_project(&temp, &home, &server);
    let (devs, public) = stores(&server);
    let base = list_of(&server, &token, "main");
    let root_a = clone_tracker(&server, &token, &temp, "root", "root-a");
    let root_b = clone_tracker(&server, &token, &temp, "root", "root-b");
    let blog_b = clone_tracker(&server, &token, &temp, "blog", "blog-b");

    // A: root's README. B: blog's post, and root's pin of it. Both on base.
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
    let b_blog = commit_on(
        &home,
        &blog_b,
        &base["blog"],
        &[("post.md", Some("hello, world\n"))],
        &[],
        "post grows",
    );
    let b_root = commit_on(
        &home,
        &root_b,
        &base["root"],
        &[],
        &[("blog", &b_blog)],
        "Update blog",
    );
    let mut b_list = base.clone();
    b_list.insert("root".into(), b_root.clone());
    b_list.insert("blog".into(), b_blog.clone());

    // A lands first: main is still its base, so it fast-forwards, ids kept.
    let (status, a) = merge_by_hand(
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
    assert_eq!(status, 200, "{a}");
    assert_eq!(a["merged"], false);
    assert_eq!(list_of(&server, &token, "main"), a_list, "fast-forwarded");

    // B, begun from the same base, lands too: merged onto A.
    let moves = serde_json::json!([
        { "tracker": "blog", "branch": "main", "old": base["blog"], "new": b_blog },
    ]);
    let (status, b) = merge_by_hand(
        &server,
        &token,
        "main",
        &base,
        &b_list,
        &[
            Sent {
                tracker: "blog",
                dir: &blog_b,
                commit: b_blog.clone(),
            },
            Sent {
                tracker: "root",
                dir: &root_b,
                commit: b_root.clone(),
            },
        ],
        moves,
    );
    assert_eq!(status, 200, "{b}");
    assert_eq!(b["merged"], true, "{b}");
    let landed = list_of(&server, &token, "main");
    assert_eq!(landed["blog"], b_blog, "only B moved blog: B's own commit");
    let merge = &landed["root"];
    assert_eq!(
        parents_of(&home, &devs, merge),
        vec![a_root.clone(), b_root.clone()],
        "the head's commit first, the merger's second"
    );
    assert_eq!(
        file_at(&home, &devs, merge, "README.md").as_deref(),
        Some("one\ntwo\nthree\nfour\n"),
        "A's work is in the merge"
    );
    assert_eq!(
        in_store(
            &home,
            &devs,
            &["rev-parse", &format!("{merge}:.newgit/pins/blog")]
        ),
        b_blog,
        "B's pin is in the merge"
    );
    let message = in_store(&home, &devs, &["log", "-1", "--format=%s", merge]);
    assert_eq!(message, "Merge `jack` into main");
    assert_eq!(blog_main(&home, &public), b_blog, "blog's main followed");
    // Root's merge commit is root's: never in the public store.
    assert!(!store_has(&public, merge));
}

#[test]
fn a_tracker_both_sides_moved_gets_one_merge_commit_of_its_own() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let _store = merge_project(&temp, &home, &server);
    let (devs, public) = stores(&server);
    let base = list_of(&server, &token, "main");
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
        let root_commit = commit_on(
            &home,
            &root,
            &base["root"],
            &[],
            &[("blog", &blog_commit)],
            "Update blog",
        );
        let mut list = base.clone();
        list.insert("root".into(), root_commit.clone());
        list.insert("blog".into(), blog_commit.clone());
        let moves = serde_json::json!([
            { "tracker": "blog", "branch": "main", "old": base["blog"], "new": blog_commit },
        ]);
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
                    commit: root_commit.clone(),
                },
            ],
            moves,
        );
        assert_eq!(status, 200, "{answer}");
        made.push((blog_commit, root_commit));
    }
    let landed = list_of(&server, &token, "main");
    let merged_blog = &landed["blog"];
    assert_eq!(
        parents_of(&home, &public, merged_blog),
        vec![made[0].0.clone(), made[1].0.clone()]
    );
    let files = in_store(
        &home,
        &public,
        &["ls-tree", "-r", "--name-only", merged_blog],
    );
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        vec!["a.md", "b.md", "post.md"],
        "blog's merge commit holds blog's files and nothing else"
    );
    assert_eq!(
        in_store(&home, &public, &["log", "-1", "--format=%B", merged_blog]),
        "Merge `jack` into main",
        "a stock message, about nobody's content"
    );
    assert_eq!(
        blog_main(&home, &public),
        *merged_blog,
        "blog's main followed to the merge"
    );
    assert_eq!(
        parents_of(&home, &devs, &landed["root"]),
        vec![made[0].1.clone(), made[1].1.clone()]
    );
}

#[test]
fn the_same_lines_changed_on_both_sides_are_refused_naming_the_file() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let _store = merge_project(&temp, &home, &server);
    let base = list_of(&server, &token, "main");
    let mut lists = Vec::new();
    for side in ["a", "b"] {
        let root = clone_tracker(&server, &token, &temp, "root", &format!("root-{side}"));
        let commit = commit_on(
            &home,
            &root,
            &base["root"],
            &[(
                "notes.md",
                Some(&format!(
                    "first line\nsecond line, by {side}\nthird line\nfourth line\nfifth line\n"
                )),
            )],
            &[],
            &format!("{side} edits notes"),
        );
        let mut list = base.clone();
        list.insert("root".into(), commit.clone());
        let answer = merge_by_hand(
            &server,
            &token,
            "main",
            &base,
            &list,
            &[Sent {
                tracker: "root",
                dir: &root,
                commit,
            }],
            serde_json::json!([]),
        );
        lists.push((list, answer));
    }
    assert_eq!(lists[0].1.0, 200, "{}", lists[0].1.1);
    let (status, refused) = &lists[1].1;
    assert_eq!(*status, 409, "{refused}");
    let message = refused["error"].as_str().unwrap_or_default();
    assert!(message.contains("`notes.md`"), "{message}");
    assert_eq!(
        list_of(&server, &token, "main"),
        lists[0].0,
        "nothing else moved"
    );
}

/// Followed into a tracker no more people read: `blog` is public, root the
/// members', so carrying B's edit from one to the other widens nothing.
/// (The other way round is refused: `serve_merge_audience_rule.rs`.)
#[test]
fn a_file_moved_between_trackers_is_followed() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let _store = merge_project(&temp, &home, &server);
    let (devs, public) = stores(&server);
    let base = list_of(&server, &token, "main");
    let post = "a first line\na second line\na third line\na fourth line\n";

    // The post, longer, so a move and an edit are each still "similar".
    let blog_0 = clone_tracker(&server, &token, &temp, "blog", "blog-0");
    let root_0 = clone_tracker(&server, &token, &temp, "root", "root-0");
    let blog_c = commit_on(
        &home,
        &blog_0,
        &base["blog"],
        &[("post.md", Some(post))],
        &[],
        "a post",
    );
    let root_c = commit_on(
        &home,
        &root_0,
        &base["root"],
        &[],
        &[("blog", &blog_c)],
        "Update blog",
    );
    let mut first = base.clone();
    first.insert("blog".into(), blog_c.clone());
    first.insert("root".into(), root_c.clone());
    let (status, answer) = merge_by_hand(
        &server,
        &token,
        "main",
        &base,
        &first,
        &[
            Sent {
                tracker: "blog",
                dir: &blog_0,
                commit: blog_c.clone(),
            },
            Sent {
                tracker: "root",
                dir: &root_0,
                commit: root_c,
            },
        ],
        serde_json::json!([
            { "tracker": "blog", "branch": "main", "old": base["blog"], "new": blog_c },
        ]),
    );
    assert_eq!(status, 200, "{answer}");
    let base = list_of(&server, &token, "main");

    // A moves blog's post.md into root.
    let blog_a = clone_tracker(&server, &token, &temp, "blog", "blog-a");
    let root_a = clone_tracker(&server, &token, &temp, "root", "root-a");
    let a_blog = commit_on(
        &home,
        &blog_a,
        &base["blog"],
        &[("post.md", None)],
        &[],
        "the post moves to root",
    );
    let a_root = commit_on(
        &home,
        &root_a,
        &base["root"],
        &[("post.md", Some(post))],
        &[("blog", &a_blog)],
        "the post moves to root",
    );
    let mut a_list = base.clone();
    a_list.insert("root".into(), a_root.clone());
    a_list.insert("blog".into(), a_blog.clone());
    let (status, answer) = merge_by_hand(
        &server,
        &token,
        "main",
        &base,
        &a_list,
        &[
            Sent {
                tracker: "blog",
                dir: &blog_a,
                commit: a_blog.clone(),
            },
            Sent {
                tracker: "root",
                dir: &root_a,
                commit: a_root.clone(),
            },
        ],
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");

    // B, from the same base, edits the post where it was.
    let blog_b = clone_tracker(&server, &token, &temp, "blog", "blog-b");
    let root_b = clone_tracker(&server, &token, &temp, "root", "root-b");
    let edited = post.replace("a fourth line", "a fourth line, edited");
    let b_blog = commit_on(
        &home,
        &blog_b,
        &base["blog"],
        &[("post.md", Some(&edited))],
        &[],
        "post: the last line",
    );
    let b_root = commit_on(
        &home,
        &root_b,
        &base["root"],
        &[],
        &[("blog", &b_blog)],
        "Update blog",
    );
    let mut b_list = base.clone();
    b_list.insert("root".into(), b_root.clone());
    b_list.insert("blog".into(), b_blog.clone());
    let (status, answer) = merge_by_hand(
        &server,
        &token,
        "main",
        &base,
        &b_list,
        &[
            Sent {
                tracker: "blog",
                dir: &blog_b,
                commit: b_blog,
            },
            Sent {
                tracker: "root",
                dir: &root_b,
                commit: b_root,
            },
        ],
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let landed = list_of(&server, &token, "main");
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "post.md").as_deref(),
        Some(edited.as_str()),
        "B's edit followed the post into root"
    );
    assert_eq!(file_at(&home, &public, &landed["blog"], "post.md"), None);
}
