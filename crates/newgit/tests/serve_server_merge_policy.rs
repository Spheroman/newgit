// A merge is held to the rules at the head it lands on, never at its base
// (#411; landing.md, *Landing* 2 and 5). `ana` maintains `blog` and begins
// a merge there; `jack` takes `blog` back before she finishes.
//
// - Finished after jack's change landed, ana's merge is refused: the rules
//   are read again at the head it would land on.
// - A change to the definitions is a barrier: it lands, merged onto the
//   current head when its base is stale, and bumps the project's epoch; a
//   change that touches no definition does not.
// - Finished while jack's change lands between ana's merge and its recheck,
//   ana's merge was prepared under the old epoch: it starts again, reads
//   the rules again, and is refused.

include!("support/serve.rs");
include!("support/serve_merge.rs");

fn add_member(server: &Server, token: &str, login: &str) {
    let bearer = format!("Bearer {token}");
    let (status, answer) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/members",
        &[("authorization", bearer.as_str())],
        &format!("{{\"login\":\"{login}\"}}"),
    );
    assert_eq!(status, 200, "{answer}");
}

fn epoch(server: &Server, token: &str) -> u64 {
    project_json(server, token)["epoch"]
        .as_u64()
        .expect("an epoch")
}

/// `blog.toml` with its maintainers `value`.
fn with_maintainers(text: &str, value: &str) -> String {
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !line.starts_with("maintainers"))
        .map(str::to_owned)
        .collect();
    lines.push(format!("maintainers = \"{value}\""));
    lines.join("\n") + "\n"
}

/// A merge by `jack` setting blog's maintainers to `value`, made on `base`:
/// access's commit and root's, each in a clone of its own.
struct Change {
    list: List,
    access: (Utf8PathBuf, String),
    root: (Utf8PathBuf, String),
}

impl Change {
    fn sent(&self) -> Vec<Sent<'_>> {
        vec![
            Sent {
                tracker: "access",
                dir: &self.access.0,
                commit: self.access.1.clone(),
            },
            Sent {
                tracker: "root",
                dir: &self.root.0,
                commit: self.root.1.clone(),
            },
        ]
    }

    fn moves(&self, base: &List) -> Value {
        serde_json::json!([
            { "tracker": "access", "branch": "main", "old": base["access"], "new": self.access.1 },
        ])
    }
}

fn maintainers(
    server: &Server,
    token: &str,
    temp: &Utf8Path,
    home: &Utf8Path,
    base: &List,
    value: &str,
    name: &str,
) -> Change {
    let access = clone_tracker(server, token, temp, "access", &format!("access-{name}"));
    let root = clone_tracker(server, token, temp, "root", &format!("root-{name}"));
    git(
        home,
        &access,
        &["checkout", "-q", "--detach", &base["access"]],
    );
    let text = std::fs::read_to_string(access.join("blog.toml")).expect("blog.toml");
    let access_commit = commit_on(
        home,
        &access,
        &base["access"],
        &[("blog.toml", Some(&with_maintainers(&text, value)))],
        &[],
        &format!("blog is maintained by {value}"),
    );
    let root_commit = commit_on(
        home,
        &root,
        &base["root"],
        &[],
        &[("access", &access_commit)],
        "Update access",
    );
    let mut list = base.clone();
    list.insert("access".into(), access_commit.clone());
    list.insert("root".into(), root_commit.clone());
    Change {
        list,
        access: (access, access_commit),
        root: (root, root_commit),
    }
}

/// ana's change to blog's post, made on `base`, and begun: its list, and
/// the clones holding it.
fn ana_begins(
    server: &Server,
    ana: &str,
    temp: &Utf8Path,
    home: &Utf8Path,
    base: &List,
) -> (List, Utf8PathBuf, Utf8PathBuf) {
    let blog = clone_tracker(server, ana, temp, "blog", "blog-ana");
    let root = clone_tracker(server, ana, temp, "root", "root-ana");
    let blog_commit = commit_on(
        home,
        &blog,
        &base["blog"],
        &[("post.md", Some("hello from ana\n"))],
        &[],
        "ana's post",
    );
    let root_commit = commit_on(
        home,
        &root,
        &base["root"],
        &[],
        &[("blog", &blog_commit)],
        "Update blog",
    );
    let mut list = base.clone();
    list.insert("blog".into(), blog_commit.clone());
    list.insert("root".into(), root_commit.clone());
    let (status, begun) = begin_by_hand(
        server,
        ana,
        "main",
        base,
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
    );
    assert_eq!(status, 200, "ana maintains blog: {begun}");
    (list, blog, root)
}

/// A project where ana is a member and maintains blog.
fn setup(server: &Server, temp: &Utf8Path) -> (Utf8PathBuf, String, String) {
    let home = temp.join("jack");
    let jack = login(server, &home, "jack");
    let ana = login(server, &temp.join("ana"), "ana");
    let _store = merge_project(temp, &home, server);
    add_member(server, &jack, "ana");
    let base = list_of(server, &jack, "main");
    let change = maintainers(server, &jack, temp, &home, &base, "user:ana", "to-ana");
    let (status, answer) = merge_by_hand(
        server,
        &jack,
        "main",
        &base,
        &change.list,
        &change.sent(),
        change.moves(&base),
    );
    assert_eq!(status, 200, "{answer}");
    (home, jack, ana)
}

#[test]
fn a_maintainer_removed_since_the_merge_began_does_not_land() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = setup(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let (ana_list, _, _) = ana_begins(&server, &ana, &temp, &home, &base);
    let before = epoch(&server, &jack);

    // A change to no definition: no new epoch.
    let root = clone_tracker(&server, &jack, &temp, "root", "root-readme");
    let readme = commit_on(
        &home,
        &root,
        &base["root"],
        &[("README.md", Some("one\ntwo\nthree\nfour\n"))],
        &[],
        "readme grows",
    );
    let mut list = base.clone();
    list.insert("root".into(), readme.clone());
    let (status, answer) = merge_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &list,
        &[Sent {
            tracker: "root",
            dir: &root,
            commit: readme,
        }],
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(epoch(&server, &jack), before);

    // jack takes blog back, begun from the same (now stale) base: a barrier,
    // merged onto the head and landed, with a new epoch.
    let change = maintainers(&server, &jack, &temp, &home, &base, "owner", "to-owner");
    let (status, answer) = merge_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &change.list,
        &change.sent(),
        change.moves(&base),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["merged"], true, "{answer}");
    assert_eq!(epoch(&server, &jack), before + 1);
    let head = list_of(&server, &jack, "main");
    assert_eq!(head["access"], change.access.1);

    // ana's merge, begun while she maintained blog, finishes now: refused.
    let (status, refused) = finish_by_hand(
        &server,
        &ana,
        "main",
        &base,
        &ana_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 403, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|error| error.contains("only jack")),
        "{refused}"
    );
    assert_eq!(
        list_of(&server, &jack, "main"),
        head,
        "nothing of ana's landed"
    );
}

#[test]
fn a_merge_prepared_under_an_older_epoch_starts_again() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let (home, jack, ana) = setup(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let (ana_list, _, _) = ana_begins(&server, &ana, &temp, &home, &base);
    let change = maintainers(&server, &jack, &temp, &home, &base, "owner", "to-owner");
    let (status, begun) =
        begin_by_hand(&server, &jack, "main", &base, &change.list, &change.sent());
    assert_eq!(status, 200, "{begun}");

    // ana's finish reads the rules (she maintains blog), and waits between
    // its merge and its recheck; jack's change lands meanwhile.
    std::fs::write(&faults, "delay-merge-by-ana\n").expect("faults");
    let finishing = {
        let (url, ana, base, list) = (
            server.url.clone(),
            ana.clone(),
            base.clone(),
            ana_list.clone(),
        );
        std::thread::spawn(move || {
            let bearer = format!("Bearer {ana}");
            http(
                &url,
                "PUT",
                "/v1/projects/jack/store/branches/main",
                &[("authorization", bearer.as_str())],
                &serde_json::json!({ "base": base, "list": list, "moves": [] }).to_string(),
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (status, answer) = finish_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &change.list,
        change.moves(&base),
    );
    assert_eq!(status, 200, "{answer}");
    let (status, refused) = finishing.join().expect("ana's finish");
    assert_eq!(status, 403, "started again, and refused: {refused}");
    assert_eq!(list_of(&server, &jack, "main"), change.list);
}
