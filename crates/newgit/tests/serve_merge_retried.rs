// A finish whose attempt loses to a landing on the same paths runs Git's
// merge again in its next attempt, on the composed views it kept (#445,
// #460 review). jack lands an edit to notes.md's fifth line; ana's finish,
// made on the base before it and editing the first line, merges with Git
// and waits before its recheck (`delay-merge-by-ana`) while jack lands an
// edit to the third. The recheck finds the same path moved, so that attempt
// is lost, and the next merges again onto jack's second: two runs of Git's
// merge in one finish, and every edit lands.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

const NOTES: &str = "first line\nsecond line\nthird line\nfourth line\nfifth line\n";

#[test]
fn an_attempt_lost_to_the_same_paths_merges_again_in_the_next() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let (home, jack, ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let edit = |edits: &[(&str, &str)]| {
        edits.iter().fold(NOTES.to_owned(), |text, (line, by)| {
            text.replace(line, &format!("{line} by {by}"))
        })
    };

    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[("notes.md", Some(&edit(&[("first line", "ana")])))],
        "ana",
    );
    let jack_fifth = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("notes.md", Some(&edit(&[("fifth line", "jack")])))],
        "jack-fifth",
    );
    let (status, answer) = finish_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jack_fifth,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let after_fifth = list_of(&server, &jack, "main");
    let jack_third = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &after_fifth,
        &[(
            "notes.md",
            Some(&edit(&[("fifth line", "jack"), ("third line", "jack")])),
        )],
        "jack-third",
    );

    // ana's finish, which merges at once (the branch moved since her base)
    // and then waits before its recheck.
    let waiting = landing_refs_of(&server, &home, "ana").len();
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
    // Her merge has made its commit (it waits at a landing ref of its own)
    // once a ref more waits in her landings.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while landing_refs_of(&server, &home, "ana").len() <= waiting {
        assert!(
            std::time::Instant::now() < until,
            "ana's first attempt never merged"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (status, answer) = finish_by_hand(
        &server,
        &jack,
        "main",
        &after_fifth,
        &jack_third,
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let (status, answer) = finishing.join().expect("joined");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["merged"], true, "{answer}");
    assert_eq!(
        answer["server_merges"], 2,
        "Git's merge ran in both attempts: {answer}"
    );

    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "notes.md").as_deref(),
        Some(
            edit(&[
                ("first line", "ana"),
                ("third line", "jack"),
                ("fifth line", "jack"),
            ])
            .as_str()
        ),
        "every edit landed"
    );
    assert_eq!(
        landing_refs_of(&server, &home, "ana"),
        Vec::<String>::new(),
        "the attempt that lost left nothing waiting"
    );
    in_store(&home, &devs, &["fsck", "--no-dangling"]);
}
