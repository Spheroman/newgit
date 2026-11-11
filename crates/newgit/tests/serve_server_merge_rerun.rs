// A directory renamed by a landing reruns the merge of a landing prepared
// before it (#411; landing.md, *Landing* 8). `ana` adds `docs/new.md`; her
// finish has merged (a fast-forward then) and waits before its recheck while
// `jack`'s rename of `docs/` to `guide/` lands. Her recheck finds the branch
// moved, so her merge runs again onto the rename, and her file follows it.

include!("support/serve.rs");
include!("support/serve_merge.rs");

#[test]
fn a_rename_landed_meanwhile_reruns_the_merge_and_the_new_file_follows_it() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let home = temp.join("jack");
    let jack = login(&server, &home, "jack");
    let ana = login(&server, &temp.join("ana"), "ana");
    let _store = merge_project(&temp, &home, &server);
    let bearer = format!("Bearer {jack}");
    let (status, answer) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/members",
        &[("authorization", bearer.as_str())],
        "{\"login\":\"ana\"}",
    );
    assert_eq!(status, 200, "{answer}");
    let base = list_of(&server, &jack, "main");

    let ana_root = clone_tracker(&server, &ana, &temp, "root", "root-ana");
    let added = commit_on(
        &home,
        &ana_root,
        &base["root"],
        &[("docs/new.md", Some("new page\n"))],
        &[],
        "a new page",
    );
    let mut ana_list = base.clone();
    ana_list.insert("root".into(), added.clone());
    let (status, begun) = begin_by_hand(
        &server,
        &ana,
        "main",
        &base,
        &ana_list,
        &[Sent {
            tracker: "root",
            dir: &ana_root,
            commit: added.clone(),
        }],
    );
    assert_eq!(status, 200, "{begun}");

    let jack_root = clone_tracker(&server, &jack, &temp, "root", "root-jack");
    let renamed = commit_on(
        &home,
        &jack_root,
        &base["root"],
        &[("docs", None), ("guide/guide.md", Some("how to\n"))],
        &[],
        "docs are the guide now",
    );
    let mut jack_list = base.clone();
    jack_list.insert("root".into(), renamed.clone());
    let (status, begun) = begin_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jack_list,
        &[Sent {
            tracker: "root",
            dir: &jack_root,
            commit: renamed.clone(),
        }],
    );
    assert_eq!(status, 200, "{begun}");

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
        &jack_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["merged"], false,
        "jack's rename fast-forwarded: {answer}"
    );
    let (status, answer) = finishing.join().expect("ana's finish");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["merged"], true,
        "ana's merge ran again onto the rename: {answer}"
    );

    let devs = server.data.join("homes/jack/store/stores/project-devs.git");
    let landed = list_of(&server, &jack, "main");
    assert_eq!(
        parents_of(&home, &devs, &landed["root"]),
        vec![renamed, added]
    );
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "guide/new.md").as_deref(),
        Some("new page\n"),
        "the new file followed the rename"
    );
    assert_eq!(file_at(&home, &devs, &landed["root"], "docs/new.md"), None);
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "docs/guide.md"),
        None
    );
}
