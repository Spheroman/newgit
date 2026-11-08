//! A merge that lands below root says where, and that root rolls up to it
//! (docs/design/anchoring.md, build order step 4). A remote from before
//! anchors says nothing, and neither does the merge.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn merge_once(fixture: &Fixture, work: &str) -> String {
    std::fs::write(fixture.store.join("change.txt"), work).expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "work"]);
    fixture.ok(&fixture.store, &["merge", "main"])
}

#[test]
fn a_merge_landed_below_root_says_where() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.remote.state.lock().expect("state").anchor = Some("t3code__repos".to_owned());
    let merged = merge_once(&fixture, "one\n");
    assert!(
        merged.contains("landed at `t3code/repos`; the trackers above it roll up to it"),
        "{merged}"
    );
}

#[test]
fn a_merge_at_root_says_nothing_of_anchors() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.remote.state.lock().expect("state").anchor = Some("root".to_owned());
    assert!(!merge_once(&fixture, "one\n").contains("landed at"));
    fixture.remote.state.lock().expect("state").anchor = None;
    assert!(!merge_once(&fixture, "two\n").contains("landed at"));
}
