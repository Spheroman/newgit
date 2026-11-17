// What `newgit serve` tells a mirror it has when it fetches from it: only the
// commits of the target's view, never a tracker's own history. The scratch
// repository a pull or an export fetches into holds every tracker's
// history, the withheld ones' too; offered as `have` lines, their ids would
// reach GitHub, and since the mirror has none of them, everything would be
// fetched again each time.

include!("support/serve.rs");
include!("support/serve_mirror.rs");

fn api(server: &Server, token: &str, path: &str) -> (u16, Value) {
    let bearer = format!("Bearer {token}");
    http(
        &server.url,
        "GET",
        &format!("/v1/projects/jack/store{path}"),
        &[("authorization", bearer.as_str())],
        "",
    )
}

/// Each tracker's commit on `main`.
fn main_list(server: &Server, token: &str) -> Vec<String> {
    let (status, answer) = api(server, token, "/branches");
    assert_eq!(status, 200, "{answer}");
    answer["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|branch| branch["name"] == "main")
        .and_then(|branch| branch["list"].as_object())
        .unwrap_or_else(|| panic!("no main: {answer}"))
        .values()
        .filter_map(|commit| commit.as_str().map(str::to_owned))
        .collect()
}

/// Every object id the server's mirror Git said it has, fetching from the
/// mirror (the stand-in's side of the wire, in the same trace, only repeats
/// them).
fn haves(trace: &Utf8Path) -> Vec<String> {
    let mut said: Vec<String> = std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            line.split_once("> have ")
                .map(|(_, id)| id.trim().to_owned())
        })
        .collect();
    said.sort();
    said.dedup();
    said
}

#[test]
fn fetches_offer_the_mirror_only_the_views_commits() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let trace = temp.join("packets");
    let server = start_mirrored_with(
        &temp,
        &github,
        &no_api(),
        &[("NEWGIT_SERVE_MIRROR_TRACE_PACKET", trace.as_str())],
    );
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    let exported = mirror_main_until(&mirror, |tip| !tip.is_empty());

    // Work in the tracker the mirror withholds, landed here.
    std::fs::write(store.join("apps/t3code/app.ts"), "export const t = 2\n").expect("write");
    git(&home, &store, &["commit", "-q", "-am", "t3code only"]);
    newgit(&home, &store, &["merge", "main"]);
    let ours = main_list(&server, &token);

    // A commit pushed to the mirror and pulled: the pull fetches, and so
    // does the export that follows.
    let work = mirror_clone(&home, &temp, &mirror, "work");
    std::fs::write(work.join("README.md"), "store, from the mirror\n").expect("write");
    git(&home, &work, &["commit", "-q", "-am", "readme there"]);
    git(&home, &work, &["push", "-q", "origin", "main"]);
    let pushed = git(&home, &work, &["rev-parse", "HEAD"]);
    let (status, answer) = deliver(&server, "push", "d-1", &push_event(&pushed, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "landed", "{done}");

    let said = haves(&trace);
    assert!(
        said.contains(&exported),
        "the view's commit is offered, so nothing is fetched twice: {said:?}"
    );
    for commit in main_list(&server, &token).iter().chain(&ours) {
        assert!(
            !said.contains(commit),
            "a tracker's own commit {commit} was offered to the mirror: {said:?}"
        );
    }
    // Every id offered is one the mirror has: what it was shown is its own.
    for commit in &said {
        let known = run(
            "git",
            &home,
            &mirror,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        );
        assert!(known.status.success(), "{commit} is not the mirror's");
    }
}
