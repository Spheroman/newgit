// The recheck follows the paths a merge changes, not the whole branch
// (#411 review, blocker 4; landing.md, *Landing* 3 and 8). ana's finish
// merges, then waits before its recheck (`delay-merge-by-ana`) while jack
// lands. A landing that changed other paths is re-made under it without
// Git's merge; one that changed the same paths, or renamed a directory ana
// added into, runs her merge again. The answer's `server_merges` counts the
// runs of Git's merge.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

const NOTES: &str = "first line\nsecond line\nthird line\nfourth line\nfifth line\n";

/// ana's finish of `list` in a thread, delayed before its recheck.
fn finish_later(
    server: &Server,
    faults: &Utf8Path,
    ana: &str,
    base: &List,
    list: &List,
) -> std::thread::JoinHandle<(u16, Value)> {
    std::fs::write(faults, "delay-merge-by-ana\n").expect("faults");
    let (url, ana, base, list) = (
        server.url.clone(),
        ana.to_owned(),
        base.clone(),
        list.clone(),
    );
    let finishing = std::thread::spawn(move || {
        let bearer = format!("Bearer {ana}");
        http(
            &url,
            "PUT",
            "/v1/projects/jack/store/branches/main",
            &[("authorization", bearer.as_str())],
            &serde_json::json!({ "base": base, "list": list, "moves": [] }).to_string(),
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(400));
    finishing
}

fn setup(temp: &Utf8Path) -> (Server, Utf8PathBuf, Utf8PathBuf, String, String) {
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(temp, &faults);
    let (home, jack, ana) = two_people(&server, temp);
    (server, faults, home, jack, ana)
}

/// ana adds a page to `blog`; jack, meanwhile, a file to root. Hers is
/// re-made on his without Git's merge: root's commit has his first and
/// hers second, and blog keeps her own commit.
#[test]
fn a_landing_on_other_paths_is_re_made_without_merging_again() {
    let (_guard, temp) = tempdir();
    let (server, faults, home, jack, ana) = setup(&temp);
    set_blog(
        &server,
        &jack,
        &temp,
        &home,
        &[("maintainers", "project-devs")],
        "members",
    );
    let base = list_of(&server, &jack, "main");
    let blog = clone_tracker(&server, &ana, &temp, "blog", "blog-ana");
    let root = clone_tracker(&server, &ana, &temp, "root", "root-ana");
    let ana_blog = commit_on(
        &home,
        &blog,
        &base["blog"],
        &[("ana.md", Some("ana\n"))],
        &[],
        "ana's page",
    );
    let ana_root = commit_on(
        &home,
        &root,
        &base["root"],
        &[],
        &[("blog", &ana_blog)],
        "Update blog",
    );
    let mut ana_list = base.clone();
    ana_list.insert("blog".into(), ana_blog.clone());
    ana_list.insert("root".into(), ana_root.clone());
    let (status, begun) = begin_by_hand(
        &server,
        &ana,
        "main",
        &base,
        &ana_list,
        &[
            Sent {
                tracker: "blog",
                dir: &blog,
                commit: ana_blog.clone(),
            },
            Sent {
                tracker: "root",
                dir: &root,
                commit: ana_root.clone(),
            },
        ],
    );
    assert_eq!(status, 200, "{begun}");
    let jack_list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("jack.md", Some("jack\n"))],
        "jack",
    );

    let finishing = finish_later(&server, &faults, &ana, &base, &ana_list);
    let (status, answer) = finish_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jack_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let (status, answer) = finishing.join().expect("joined");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["merged"], true, "{answer}");
    assert_eq!(
        answer["server_merges"], 0,
        "no run of Git's merge: {answer}"
    );

    let landed = list_of(&server, &jack, "main");
    assert_eq!(landed["blog"], ana_blog, "blog keeps ana's own commit");
    let (devs, _) = devs_and_public(&server);
    assert_eq!(
        parents_of(&home, &devs, &landed["root"]),
        vec![jack_list["root"].clone(), ana_root],
        "root's commit is on jack's, with ana's second"
    );
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "jack.md").as_deref(),
        Some("jack\n")
    );
    assert_eq!(
        landing_refs_of(&server, &home, "ana"),
        Vec::<String>::new(),
        "nothing waits in ana's landings"
    );
}

/// The same from the first attempt: jack landed before ana's finish began,
/// on other paths, so her merge is made on his without Git's merge at all.
#[test]
fn a_merge_onto_other_paths_never_runs_git_merge() {
    let (_guard, temp) = tempdir();
    let (server, _faults, home, jack, ana) = setup(&temp);
    let base = list_of(&server, &jack, "main");
    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[("ana.md", Some("ana\n"))],
        "ana",
    );
    let jack_list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("jack.md", Some("jack\n"))],
        "jack",
    );
    let (status, answer) = finish_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jack_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let (status, answer) = finish_by_hand(
        &server,
        &ana,
        "main",
        &base,
        &ana_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["merged"], true, "{answer}");
    assert_eq!(answer["server_merges"], 0, "{answer}");
    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    for (file, text) in [("ana.md", "ana\n"), ("jack.md", "jack\n")] {
        assert_eq!(
            file_at(&home, &devs, &landed["root"], file).as_deref(),
            Some(text)
        );
    }
}

/// ana edits notes.md's first line; jack, meanwhile, its last. The same
/// path: her merge runs again, once, and both lines land.
#[test]
fn a_landing_on_the_same_paths_runs_the_merge_again() {
    let (_guard, temp) = tempdir();
    let (server, faults, home, jack, ana) = setup(&temp);
    let base = list_of(&server, &jack, "main");
    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[(
            "notes.md",
            Some(&NOTES.replace("first line", "first line by ana")),
        )],
        "ana",
    );
    let jack_list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[(
            "notes.md",
            Some(&NOTES.replace("fifth line", "fifth line by jack")),
        )],
        "jack",
    );
    let finishing = finish_later(&server, &faults, &ana, &base, &ana_list);
    let (status, answer) = finish_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jack_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let (status, answer) = finishing.join().expect("joined");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["server_merges"], 1, "merged again, once: {answer}");
    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "notes.md").as_deref(),
        Some(
            NOTES
                .replace("first line", "first line by ana")
                .replace("fifth line", "fifth line by jack")
                .as_str()
        )
    );
    assert_eq!(
        landing_refs_of(&server, &home, "ana"),
        Vec::<String>::new(),
        "the attempt that lost left nothing waiting"
    );
}

/// ana adds `docs/new.md`; jack, meanwhile, renames `docs/` to `guide/`.
/// Different files, but the directory she added into is gone: her merge
/// runs again, and her page follows the rename.
#[test]
fn a_directory_renamed_meanwhile_overlaps_everything_under_it() {
    let (_guard, temp) = tempdir();
    let (server, faults, home, jack, ana) = setup(&temp);
    let base = list_of(&server, &jack, "main");
    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[("docs/new.md", Some("new page\n"))],
        "ana",
    );
    let jack_list = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("docs", None), ("guide/guide.md", Some("how to\n"))],
        "jack",
    );
    let finishing = finish_later(&server, &faults, &ana, &base, &ana_list);
    let (status, answer) = finish_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jack_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let (status, answer) = finishing.join().expect("joined");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["server_merges"], 1, "{answer}");
    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "guide/new.md").as_deref(),
        Some("new page\n")
    );
    assert_eq!(file_at(&home, &devs, &landed["root"], "docs/new.md"), None);
}
