//! A remote with no pull request routes (newgit.dev's Worker, or a `newgit
//! serve` from before them) is said to be one, not taken for a missing pull
//! request.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn pr_commands_on_a_remote_without_pull_requests_say_so() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    for args in [&["pr", "list"][..], &["pr", "checkout", "1"][..]] {
        let output = fixture.newgit(&fixture.store, args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            said.contains("this remote has no pull requests yet"),
            "{args:?}: {said}"
        );
    }
}
