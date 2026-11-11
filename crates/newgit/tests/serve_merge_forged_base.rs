// A merge's base is what its root commit pins, read by the server; a list
// the merger wrote as its base is refused, never trusted for a decision
// (#411 review, blocker 1).

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

/// ana maintains only `blog`. She names a base whose `access` is the commit
/// before the current one and a list with that same `access`, so a diff of
/// her base and list says she moves only `blog` and root, while what lands
/// would move `access` back, reverting the line that lets her maintain blog.
#[test]
fn a_base_that_root_does_not_pin_is_refused() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = two_people(&server, &temp);
    set_blog(
        &server,
        &jack,
        &temp,
        &home,
        &[("maintainers", "user:ana")],
        "to-ana",
    );
    let current = list_of(&server, &jack, "main");
    let epoch_before = epoch(&server, &jack);

    let access = clone_tracker(&server, &ana, &temp, "access", "access-ana");
    let older_access = git(
        &home,
        &access,
        &["rev-parse", &format!("{}^", current["access"])],
    );
    let blog = clone_tracker(&server, &ana, &temp, "blog", "blog-ana");
    let root = clone_tracker(&server, &ana, &temp, "root", "root-ana");
    let blog_commit = commit_on(
        &home,
        &blog,
        &current["blog"],
        &[("post.md", Some("ana\n"))],
        &[],
        "ana's post",
    );
    let root_commit = commit_on(
        &home,
        &root,
        &current["root"],
        &[],
        &[("blog", &blog_commit), ("access", &older_access)],
        "Update blog",
    );
    let mut list = current.clone();
    list.insert("blog".into(), blog_commit.clone());
    list.insert("root".into(), root_commit.clone());
    list.insert("access".into(), older_access.clone());
    let mut forged = current.clone();
    forged.insert("access".into(), older_access);

    let (status, answer) = merge_by_hand(
        &server,
        &ana,
        "main",
        &forged,
        &list,
        &[
            Sent {
                tracker: "blog",
                dir: &blog,
                commit: blog_commit,
            },
            Sent {
                tracker: "root",
                dir: &root,
                commit: root_commit,
            },
        ],
        serde_json::json!([]),
    );
    assert_eq!(status, 409, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or_default()
            .contains("is not what root's commit"),
        "{answer}"
    );
    assert_eq!(list_of(&server, &jack, "main"), current, "nothing landed");
    assert_eq!(epoch(&server, &jack), epoch_before);

    // The same change on the true base is what moves `access`, which ana
    // does not maintain.
    let (status, answer) = begin_by_hand(&server, &ana, "main", &current, &list, &[]);
    assert_eq!(status, 403, "{answer}");
}

/// A finish naming another base than its begin did is refused too: the
/// base is the one the server read when the merge began.
#[test]
fn a_finish_on_another_base_is_refused() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, _ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("a.md", Some("a\n"))],
        "a",
    );
    let mut other = base.clone();
    other.insert("blog".into(), base["root"].clone());
    let (status, answer) =
        finish_by_hand(&server, &jack, "main", &other, &list, serde_json::json!([]));
    assert_eq!(status, 400, "{answer}");
    let (status, answer) =
        finish_by_hand(&server, &jack, "main", &base, &list, serde_json::json!([]));
    assert_eq!(status, 200, "{answer}");
}
