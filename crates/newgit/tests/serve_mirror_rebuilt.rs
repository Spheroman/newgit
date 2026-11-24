// #451 (a), as it stood on newgit.dev: `main` here had taken the mirror's
// last merge by `newgit pull --remote` and a merge of its own (so `main`
// was a merge commit the mirror never made, differing from the mirror only
// in what the mirror does not ship), and the next pull request was built on
// the mirror's `main` as it was, changing a tracker mounted below root.
// The Worker refused it for conflicts in files nobody had changed twice. On
// `newgit serve` it lands: the pull is the server's merge from the root
// commit the mirror's history left, not a merge of one tracker's histories.

include!("support/serve.rs");
include!("support/serve_mirror.rs");

#[test]
fn a_branch_built_on_a_merge_main_took_by_hand_lands_without_a_spurious_conflict() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    // A pull request merged on the mirror, changing the start of
    // `crates/serve/a.rs`. Its event never lands it.
    let work = mirror_clone(&home, &temp, &mirror, "pr");
    git(&home, &work, &["checkout", "-q", "-b", "earlier"]);
    let a = std::fs::read_to_string(work.join("crates/serve/a.rs")).expect("a.rs");
    std::fs::write(
        work.join("crates/serve/a.rs"),
        a.replace("fn one() {}", "fn one() { 1 }"),
    )
    .expect("write");
    git(&home, &work, &["commit", "-q", "-am", "one returns one"]);
    git(&home, &work, &["checkout", "-q", "main"]);
    git(
        &home,
        &work,
        &[
            "merge",
            "-q",
            "--no-ff",
            "earlier",
            "-m",
            "Merge pull request #1 from jack/earlier",
        ],
    );
    git(&home, &work, &["push", "-q", "origin", "main"]);
    let earlier = git(&home, &work, &["rev-parse", "HEAD"]);

    // Here, `main` moves in what the mirror does not ship, then takes the
    // mirror's merge by hand: a merge commit of its own.
    std::fs::write(store.join("apps/t3code/app.ts"), "export const t3 = 1\n").expect("write");
    git(&home, &store, &["commit", "-q", "-am", "here: t3code"]);
    newgit(&home, &store, &["merge", "main"]);
    newgit(&home, &store, &["pull", "--remote", "mirror"]);
    newgit(&home, &store, &["merge", "main"]);
    let a = main_file(&server, &token, &temp, "crates/serve/a.rs");
    assert!(
        a.contains("fn one() { 1 }"),
        "the mirror's merge is on main: {a}"
    );
    let until = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while state(&server, &token)["exports"][0]["state"] != "up to date" {
        assert!(
            std::time::Instant::now() < until,
            "the export never caught up: {}",
            state(&server, &token)
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // The next pull request: one commit built on the mirror's `main` as it
    // was at that merge, changing the end of the same file, merged onto the
    // mirror's `main` as it is now.
    git(&home, &work, &["checkout", "-q", "-b", "fix", &earlier]);
    let a = std::fs::read_to_string(work.join("crates/serve/a.rs")).expect("a.rs");
    std::fs::write(
        work.join("crates/serve/a.rs"),
        a.replace("fn five() {}", "fn five() { 5 }"),
    )
    .expect("write");
    git(&home, &work, &["commit", "-q", "-am", "five returns five"]);
    git(&home, &work, &["fetch", "-q", "origin"]);
    git(
        &home,
        &work,
        &["checkout", "-q", "-B", "main", "origin/main"],
    );
    git(
        &home,
        &work,
        &[
            "merge",
            "-q",
            "--no-ff",
            "fix",
            "-m",
            "Merge pull request #2 from jack/fix",
        ],
    );
    git(&home, &work, &["push", "-q", "origin", "main"]);
    let merged = git(&home, &work, &["rev-parse", "HEAD"]);

    let (status, answer) = deliver(&server, "push", "d-2", &push_event(&merged, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "landed", "{done}");
    let a = main_file(&server, &token, &temp, "crates/serve/a.rs");
    assert!(a.contains("fn one() { 1 }"), "{a}");
    assert!(a.contains("fn five() { 5 }"), "{a}");
    assert_eq!(
        main_file(&server, &token, &temp, "apps/t3code/app.ts"),
        "export const t3 = 1\n"
    );
    // And the mirror keeps the merged pull request as itself.
    let tip = mirror_main_until(&mirror, |tip| ancestor(&mirror, &merged, tip));
    let files = git(&home, &mirror, &["ls-tree", "-r", "--name-only", &tip]);
    assert!(!files.contains("apps/t3code"), "{files}");
}
