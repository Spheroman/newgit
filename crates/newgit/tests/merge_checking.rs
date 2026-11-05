//! The remote checks a merge's index in the background: the first time a
//! large tracker is there (Spheroman/t3code, 365 MiB) the check takes longer
//! than any one request should wait, so it answers `202 {"checking": true}`
//! until it is done. `newgit merge` asks again until it gets the answer (#144).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn a_merge_whose_finish_is_still_checking_asks_again_until_it_lands() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);

    std::fs::write(fixture.store.join("change.txt"), "work\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "work"]);

    let finishes = |requests: &[(String, String, String)]| {
        requests
            .iter()
            .filter(|(method, path, _)| {
                method == "PUT" && path == "/v1/projects/jack/store/branches/main"
            })
            .count()
    };
    let before = finishes(&fixture.remote.requests());
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .checking_finishes = 2;
    fixture.ok(&fixture.store, &["merge", "main"]);

    let state = fixture.remote.state.lock().expect("state");
    assert_eq!(state.checking_finishes, 0, "both 202s were answered");
    assert_eq!(
        finishes(&state.requests) - before,
        3,
        "two answers that it was still checking, then the real one"
    );
    let pushed = git(
        &fixture.remote.repo("jack.store.t.root"),
        &["rev-parse", "main"],
    );
    assert_eq!(
        state.branches["jack/store"]["main"]["root"].as_str(),
        Some(pushed.as_str()),
        "the merge landed"
    );
}
