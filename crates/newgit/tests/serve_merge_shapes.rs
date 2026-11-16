// Server merges over what Git tracks besides lines of text (#411 review,
// item 11): a mode change, a symlink, a binary file, a merge with nothing
// left to do, and a criss-cross history. jack lands first; ana's merge,
// begun from the same base, is merged onto his.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

/// A commit on `parent` in clone `dir`, made by `change` (which edits the
/// work tree and the index there).
fn commit_by(
    home: &Utf8Path,
    dir: &Utf8Path,
    parent: &str,
    change: impl FnOnce(),
    message: &str,
) -> String {
    git(home, dir, &["checkout", "-q", "--detach", parent]);
    change();
    git(home, dir, &["commit", "-q", "--allow-empty", "-m", message]);
    git(home, dir, &["rev-parse", "HEAD"])
}

/// Begin `commit` (root's, on `base`) as `token`'s caller: its list.
fn begin_root(server: &Server, token: &str, base: &List, dir: &Utf8Path, commit: &str) -> List {
    let mut list = base.clone();
    list.insert("root".into(), commit.to_owned());
    let (status, begun) = begin_by_hand(
        server,
        token,
        "main",
        base,
        &list,
        &[Sent {
            tracker: "root",
            dir,
            commit: commit.to_owned(),
        }],
    );
    assert_eq!(status, 200, "{begun}");
    list
}

fn land(server: &Server, token: &str, base: &List, list: &List) -> (u16, Value) {
    finish_by_hand(server, token, "main", base, list, serde_json::json!([]))
}

/// `path`'s mode at root's `commit`.
fn mode_of(home: &Utf8Path, store: &Utf8Path, commit: &str, path: &str) -> String {
    in_store(home, store, &["ls-tree", commit, path])
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// jack edits notes.md's last line; ana makes it executable. The same
/// path, so Git merges them: his content, her mode.
#[test]
fn a_mode_change_merges_with_an_edit() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let notes = "first line\nsecond line\nthird line\nfourth line\nfifth line\n";
    let edited = notes.replace("fifth line", "fifth line, edited");
    let jack_list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("notes.md", Some(&edited))],
        "jack",
    );
    let clone = clone_tracker(&server, &ana, &temp, "root", "root-ana");
    let ana_commit = commit_by(
        &home,
        &clone,
        &base["root"],
        || {
            git(&home, &clone, &["update-index", "--chmod=+x", "notes.md"]);
        },
        "notes.md runs",
    );
    let ana_list = begin_root(&server, &ana, &base, &clone, &ana_commit);
    assert_eq!(land(&server, &jack, &base, &jack_list).0, 200);
    let (status, answer) = land(&server, &ana, &base, &ana_list);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["server_merges"], 1, "{answer}");
    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "notes.md").as_deref(),
        Some(edited.as_str())
    );
    assert_eq!(mode_of(&home, &devs, &landed["root"], "notes.md"), "100755");
}

/// ana adds a symlink; jack, meanwhile, edits the README. Re-made on his
/// commit without Git's merge, her link is still a link.
#[test]
fn a_symlink_lands_as_a_symlink() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let jack_list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("README.md", Some("one\ntwo\nthree\nfour\n"))],
        "jack",
    );
    let clone = clone_tracker(&server, &ana, &temp, "root", "root-ana");
    let ana_commit = commit_by(
        &home,
        &clone,
        &base["root"],
        || {
            std::os::unix::fs::symlink("notes.md", clone.join("latest")).expect("symlink");
            git(&home, &clone, &["add", "latest"]);
        },
        "a link to the notes",
    );
    let ana_list = begin_root(&server, &ana, &base, &clone, &ana_commit);
    assert_eq!(land(&server, &jack, &base, &jack_list).0, 200);
    let (status, answer) = land(&server, &ana, &base, &ana_list);
    assert_eq!(status, 200, "{answer}");
    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    assert_eq!(mode_of(&home, &devs, &landed["root"], "latest"), "120000");
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "latest").as_deref(),
        Some("notes.md")
    );
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "README.md").as_deref(),
        Some("one\ntwo\nthree\nfour\n")
    );
}

/// Both change one binary file: a conflict, named.
#[test]
fn a_binary_file_changed_on_both_sides_is_a_named_conflict() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = two_people(&server, &temp);
    let first = list_of(&server, &jack, "main");
    let added = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &first,
        &[("logo.bin", Some("\0\u{1}\u{2}logo\0\u{3}"))],
        "logo",
    );
    assert_eq!(land(&server, &jack, &first, &added).0, 200);
    let base = list_of(&server, &jack, "main");
    let jack_list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("logo.bin", Some("\0\u{1}\u{2}jack\0\u{3}"))],
        "jack-logo",
    );
    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[("logo.bin", Some("\0\u{1}\u{2}ana\0\u{3}"))],
        "ana-logo",
    );
    assert_eq!(land(&server, &jack, &base, &jack_list).0, 200);
    let (status, answer) = land(&server, &ana, &base, &ana_list);
    assert_eq!(status, 409, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or_default()
            .contains("`logo.bin`"),
        "{answer}"
    );
}

/// Both add the same file with the same content: Git's merge has nothing
/// to do. ana's merge still lands, recording her commit, and the file is
/// as both made it.
#[test]
fn a_merge_with_nothing_left_to_do_lands_her_history() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let jack_list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("same.md", Some("same\n"))],
        "jack",
    );
    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[("same.md", Some("same\n"))],
        "ana",
    );
    assert_eq!(land(&server, &jack, &base, &jack_list).0, 200);
    let (status, answer) = land(&server, &ana, &base, &ana_list);
    assert_eq!(status, 200, "{answer}");
    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "same.md").as_deref(),
        Some("same\n")
    );
    let ancestor = run(
        "git",
        &home,
        &devs,
        &[
            "--git-dir",
            devs.as_str(),
            "merge-base",
            "--is-ancestor",
            &ana_list["root"],
            &landed["root"],
        ],
    );
    assert!(ancestor.status.success(), "ana's commit is in what landed");
    // Asked again, the finish says it landed.
    let (status, answer) = land(&server, &ana, &base, &ana_list);
    assert_eq!(status, 200, "{answer}");
}

/// jack lands a merge of two lines of work, P then Q; ana's, from the same
/// base, merges them the other way round, Q then P: two merge bases. The
/// server merges against the base the merge names, and both sides' own
/// files land.
#[test]
fn a_criss_cross_history_merges_on_the_named_base() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let clone = clone_tracker(&server, &jack, &temp, "root", "root-jack");
    let p = commit_on(
        &home,
        &clone,
        &base["root"],
        &[("p.md", Some("p\n"))],
        &[],
        "P",
    );
    let q = commit_on(
        &home,
        &clone,
        &base["root"],
        &[("q.md", Some("q\n"))],
        &[],
        "Q",
    );
    let crossed = |first: &str, second: &str, own: &str| {
        git(&home, &clone, &["checkout", "-q", "--detach", first]);
        git(&home, &clone, &["merge", "-q", "--no-edit", second]);
        std::fs::write(clone.join(own), format!("{own}\n")).expect("write");
        git(&home, &clone, &["add", own]);
        git(&home, &clone, &["commit", "-q", "--amend", "--no-edit"]);
        git(&home, &clone, &["rev-parse", "HEAD"])
    };
    let x = crossed(&p, &q, "x.md");
    let y = crossed(&q, &p, "y.md");
    let jack_list = begin_root(&server, &jack, &base, &clone, &x);
    assert_eq!(land(&server, &jack, &base, &jack_list).0, 200);
    let ana_clone = clone_tracker(&server, &ana, &temp, "root", "root-ana");
    git(&home, &ana_clone, &["fetch", "-q", clone.as_str(), &y]);
    let ana_list = begin_root(&server, &ana, &base, &ana_clone, &y);
    let (status, answer) = land(&server, &ana, &base, &ana_list);
    assert_eq!(status, 200, "{answer}");
    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    for file in ["p.md", "q.md", "x.md", "y.md"] {
        assert!(
            file_at(&home, &devs, &landed["root"], file).is_some(),
            "{file} landed"
        );
    }
    assert_eq!(parents_of(&home, &devs, &landed["root"]), vec![x, y]);
}
