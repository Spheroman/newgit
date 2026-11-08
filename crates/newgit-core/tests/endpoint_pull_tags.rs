//! `newgit endpoint-pull` for a tag made on a mirror: it lands nothing, and
//! says which root commit on `main` the tagged commit stands for, so the
//! Worker can make the project's own tag. The mirror's commits are `main`
//! composed as it sees it, so a tagged commit is found under its own id.

mod imports;
mod mirror_remote;

use std::collections::BTreeMap;

use imports::*;
use mirror_remote::*;
use newgit_core::endpoint::{PullAnswer, PullPlan, TagAnnotation, pull_mirror};

fn tag_plan(remote: &Remote, list: &BTreeMap<String, String>, name: &str) -> PullPlan {
    let object = git(&remote.mirror, &["rev-parse", &format!("refs/tags/{name}")]);
    let mut plan = remote.plan(list, &object);
    plan.tag = Some(name.to_owned());
    plan
}

#[test]
fn a_tag_on_a_commit_from_here_names_mains_root_commit() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    git(&remote.mirror, &["tag", "v1.0.0", "main"]);

    let answer = pull_mirror(&tag_plan(&remote, &base, "v1.0.0")).expect("look");
    assert_eq!(
        answer,
        PullAnswer::Tagged {
            root: remote.main("root"),
            annotation: None
        }
    );
    // Nothing landed: root's `main` is where it was.
    assert_eq!(remote.main("root"), base["root"]);
}

#[test]
fn a_tag_on_a_merged_pull_request_names_the_root_commit_it_landed_as() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let theirs = merge_pr(
        &temp,
        &remote.mirror,
        "main",
        &[("pr.txt", "pr\n")],
        "release",
    );
    let list = remote.landed(&base, &theirs);
    git_env(
        &remote.mirror,
        &["tag", "-a", "v2.0.0", "-m", "Release 2.0.0", &theirs],
        &[
            ("GIT_COMMITTER_NAME", "Pat".to_owned()),
            ("GIT_COMMITTER_EMAIL", "pat@example.com".to_owned()),
            ("GIT_COMMITTER_DATE", "1700000000 +0000".to_owned()),
        ],
    );

    let answer = pull_mirror(&tag_plan(&remote, &list, "v2.0.0")).expect("look");
    assert_eq!(
        answer,
        PullAnswer::Tagged {
            root: list["root"].clone(),
            annotation: Some(TagAnnotation {
                tagger: "Pat <pat@example.com> 1700000000 +0000".to_owned(),
                message: "Release 2.0.0\n".to_owned(),
            }),
        }
    );
}

#[test]
fn a_tag_on_a_commit_main_does_not_have_yet_is_refused() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let theirs = merge_pr(
        &temp,
        &remote.mirror,
        "main",
        &[("pr.txt", "pr\n")],
        "unlanded",
    );
    git(&remote.mirror, &["tag", "v3", &theirs]);

    match pull_mirror(&tag_plan(&remote, &base, "v3")).expect("look") {
        PullAnswer::Refused { why } => assert!(why.contains("not on `main`"), "{why}"),
        other => panic!("{other:?}"),
    }
}
