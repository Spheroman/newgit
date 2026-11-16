// What a server merge lands is authorized, not only the merger's own change
// (#411 review, blocker 2): a merge that follows a file another landing
// moved into a tracker the merger may not change is refused, naming it.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

/// `blog` is the owner's alone to change, and as private as root (the
/// members'), so only authorization stands between ana and blog. jack moves
/// `notes.md` from root into blog; ana, from the base before, edits root's
/// `notes.md`. Merged, her line would land in blog, in a commit by her.
#[test]
fn a_merge_carrying_an_edit_into_a_tracker_the_merger_may_not_change_is_refused() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = two_people(&server, &temp);
    set_blog(
        &server,
        &jack,
        &temp,
        &home,
        &[("maintainers", "owner"), ("audience", "project-devs")],
        "owner-only",
    );
    let base = list_of(&server, &jack, "main");

    // Control: ana changing blog herself is refused.
    let blog_ctl = clone_tracker(&server, &ana, &temp, "blog", "blog-ctl");
    let root_ctl = clone_tracker(&server, &ana, &temp, "root", "root-ctl");
    let blog_commit = commit_on(
        &home,
        &blog_ctl,
        &base["blog"],
        &[("post.md", Some("ana\n"))],
        &[],
        "x",
    );
    let root_commit = commit_on(
        &home,
        &root_ctl,
        &base["root"],
        &[],
        &[("blog", &blog_commit)],
        "x",
    );
    let mut control = base.clone();
    control.insert("blog".into(), blog_commit);
    control.insert("root".into(), root_commit);
    let (status, answer) = begin_by_hand(&server, &ana, "main", &base, &control, &[]);
    assert_eq!(status, 403, "{answer}");

    // ana edits root's notes.md, begun on the base.
    let notes = "first line\nsecond line\nthird line\nfourth line\nfifth line\n";
    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[(
            "notes.md",
            Some(&notes.replace("fifth line", "fifth line by ana")),
        )],
        "ana-notes",
    );

    // jack moves notes.md into blog, and lands.
    let blog = clone_tracker(&server, &jack, &temp, "blog", "blog-jack");
    let root = clone_tracker(&server, &jack, &temp, "root", "root-jack");
    let jack_blog = commit_on(
        &home,
        &blog,
        &base["blog"],
        &[("notes.md", Some(notes))],
        &[],
        "notes to blog",
    );
    let jack_root = commit_on(
        &home,
        &root,
        &base["root"],
        &[("notes.md", None)],
        &[("blog", &jack_blog)],
        "notes to blog",
    );
    let mut jack_list = base.clone();
    jack_list.insert("blog".into(), jack_blog.clone());
    jack_list.insert("root".into(), jack_root.clone());
    let (status, answer) = merge_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jack_list,
        &[
            Sent {
                tracker: "blog",
                dir: &blog,
                commit: jack_blog.clone(),
            },
            Sent {
                tracker: "root",
                dir: &root,
                commit: jack_root,
            },
        ],
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let landed = list_of(&server, &jack, "main");

    let (status, answer) = finish_by_hand(
        &server,
        &ana,
        "main",
        &base,
        &ana_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 403, "{answer}");
    let error = answer["error"].as_str().unwrap_or_default();
    assert!(error.contains("would change `blog`"), "{error}");
    assert_eq!(list_of(&server, &jack, "main"), landed, "nothing landed");
    let (devs, _) = devs_and_public(&server);
    assert_eq!(
        file_at(&home, &devs, &landed["blog"], "notes.md").as_deref(),
        Some(notes),
        "blog's notes.md is jack's"
    );
    assert!(
        landing_refs_of(&server, &home, "ana")
            .iter()
            .all(|found| !found.contains("blog")),
        "the merge commit the refused attempt made for blog is not left waiting: {:?}",
        landing_refs_of(&server, &home, "ana")
    );
}
