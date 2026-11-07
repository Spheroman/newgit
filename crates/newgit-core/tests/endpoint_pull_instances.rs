//! Work a mirror never saw. A branch exported for a pull request leaves out
//! the trackers the mirror withholds, so an instance's edit there is not on
//! the mirror, and landing the merged pull request would drop it without a
//! word. It is refused, naming `newgit merge <instance>`, and after that merge
//! the mirror's commit is recognised and nothing lands twice.

mod imports;
mod mirror_remote;

use std::collections::BTreeMap;

use imports::*;
use mirror_remote::*;
use newgit_core::compose::{self, ComposeOptions};
use newgit_core::endpoint::{MirrorInstance, PullAnswer, pull_mirror};

/// What the probe found: an agent edits `apps/t3code/x.ts` and `src/y.rs` in
/// one commit, the branch push strips t3code, and the pull request merges on
/// the mirror. Returns the mirror's merge commit, and the instance as the
/// Worker describes it.
fn merged_pr(remote: &Remote) -> (String, MirrorInstance) {
    let workspace = manager(&remote.store)
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    git(&workspace, &["config", "commit.gpgSign", "false"]);
    commit_as(
        &workspace,
        "ana",
        2_000_000,
        &[("apps/t3code/x.ts", "x\n"), ("src/y.rs", "y\n")],
        "edit both",
    );
    manager(&remote.store)
        .checkpoint("feature", None)
        .expect("checkpoint");
    let (store, reference) = manager(&remote.store)
        .tracker_log_target("root", Some("feature"))
        .expect("split");
    let root_commit = git(
        &remote.temp,
        &[&format!("--git-dir={store}"), "rev-parse", &reference],
    );
    let held = compose::index(&store, "root", &root_commit).expect("index");

    // The push to the mirror's branch: the instance, as the target sees it.
    let project = remote.store.paths().project_root.clone();
    let ships = |tracker: &str| tracker != "t3code";
    let view = compose::compose(
        &project,
        "root",
        &[("refs/newgit/test/branch".to_owned(), root_commit)],
        &ComposeOptions {
            readable: &ships,
            admit: None,
        },
    )
    .expect("compose")
    .remove(0)
    .tip
    .expect("a view");
    git(
        &project,
        &[
            "push",
            "-q",
            remote.mirror.as_str(),
            &format!("{view}:refs/heads/feature"),
        ],
    );

    let clone = remote.temp.join("pr");
    git(
        &remote.temp,
        &["clone", "-q", remote.mirror.as_str(), clone.as_str()],
    );
    git(&clone, &["config", "commit.gpgSign", "false"]);
    git(
        &clone,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #7 from acme/feature",
            "origin/feature",
        ],
    );
    git(&clone, &["push", "-q", "origin", "main"]);
    let theirs = git(&clone, &["rev-parse", "HEAD"]);
    (
        theirs,
        MirrorInstance {
            name: "feature".to_owned(),
            pins: BTreeMap::from([("t3code".to_owned(), held["t3code"].clone())]),
        },
    )
}

fn answer(
    remote: &Remote,
    list: &BTreeMap<String, String>,
    commit: &str,
    instances: Vec<MirrorInstance>,
    peek: bool,
) -> PullAnswer {
    let mut plan = remote.plan(list, commit);
    plan.instances = instances;
    plan.peek = peek;
    let answer = pull_mirror(&plan).expect("pull");
    remote.follow(&answer);
    answer
}

#[test]
fn a_merged_pr_with_withheld_work_is_refused_and_moves_nothing() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let (theirs, instance) = merged_pr(&remote);
    let base = remote.index();
    let root = remote.main("root");
    for peek in [true, false] {
        match answer(&remote, &base, &theirs, vec![instance.clone()], peek) {
            PullAnswer::Refused { why } => {
                assert!(why.contains("`newgit merge feature`"), "{why}");
                assert!(why.contains("redeliver"), "{why}");
                assert!(why.contains("`t3code`"), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(remote.main("root"), root);
}

#[test]
fn after_newgit_merge_the_mirrors_commit_is_recognised_and_nothing_lands_twice() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let (theirs, instance) = merged_pr(&remote);

    manager(&remote.store)
        .merge("feature", "main")
        .expect("newgit merge feature");
    remote.publish();
    let base = remote.index();
    assert_ne!(base["t3code"], "", "the pin moved");

    // The redelivery: the instance's pin is on `main` now, so nothing stops
    // it, and the merge on the mirror is all that is new.
    let list = match answer(&remote, &base, &theirs, vec![instance.clone()], false) {
        PullAnswer::Landed { list, commits, .. } => {
            assert_eq!(commits, 1, "only the mirror's merge commit");
            list
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(list["t3code"], base["t3code"], "the t3code edit is kept");
    let t3code = &remote.repos["t3code"];
    assert_eq!(git(t3code, &["show", "main:x.ts"]), "x");
    let root = &remote.repos["root"];
    assert_eq!(git(root, &["show", "main:src/y.rs"]), "y");
    let subjects = git(root, &["log", "--format=%s", "main"]);
    assert_eq!(
        subjects.lines().filter(|line| *line == "edit both").count(),
        1,
        "{subjects}"
    );

    assert_eq!(
        answer(&remote, &list, &theirs, vec![instance], false),
        PullAnswer::Nothing,
        "the mirror's commit is what composing `main` gives"
    );
}

#[test]
fn a_mirror_merge_of_withheld_work_keeps_it_whichever_side_is_first() {
    // The same, with the mirror's merge naming the instance's branch first.
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let (_, instance) = merged_pr(&remote);
    manager(&remote.store)
        .merge("feature", "main")
        .expect("newgit merge feature");
    remote.publish();
    let base = remote.index();

    let clone = temp.join("flipped");
    git(
        &temp,
        &[
            "clone",
            "-q",
            "-b",
            "feature",
            remote.mirror.as_str(),
            clone.as_str(),
        ],
    );
    git(&clone, &["config", "commit.gpgSign", "false"]);
    git(&clone, &["fetch", "-q", "origin", "main"]);
    git(
        &clone,
        &["merge", "-q", "--no-ff", "-m", "other way", "FETCH_HEAD"],
    );
    git(&clone, &["push", "-q", "--force", "origin", "HEAD:main"]);
    let theirs = git(&clone, &["rev-parse", "HEAD"]);
    match answer(&remote, &base, &theirs, vec![instance], false) {
        PullAnswer::Landed { list, .. } => assert_eq!(list["t3code"], base["t3code"]),
        other => panic!("{other:?}"),
    }
    assert_eq!(git(&remote.repos["t3code"], &["show", "main:x.ts"]), "x");
}

#[test]
fn an_instance_behind_main_has_nothing_to_lose() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let theirs = merge_pr(
        &temp,
        &remote.mirror,
        "main",
        &[("pr.txt", "pr\n")],
        "from GitHub",
    );
    let behind = MirrorInstance {
        name: "old".to_owned(),
        pins: BTreeMap::from([("t3code".to_owned(), base["t3code"].clone())]),
    };
    assert!(matches!(
        answer(&remote, &base, &theirs, vec![behind], false),
        PullAnswer::Landed { .. }
    ));
}

#[test]
fn a_fork_no_checkpoint_describes_counts_as_work() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let theirs = merge_pr(
        &temp,
        &remote.mirror,
        "main",
        &[("pr.txt", "pr\n")],
        "from GitHub",
    );
    let unknown = MirrorInstance {
        name: "new".to_owned(),
        pins: BTreeMap::from([("t3code".to_owned(), String::new())]),
    };
    assert!(matches!(
        answer(&remote, &base, &theirs, vec![unknown], false),
        PullAnswer::Refused { .. }
    ));
}

#[test]
fn a_look_says_what_a_landing_would_and_moves_nothing() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let root = remote.main("root");
    assert_eq!(
        answer(&remote, &base, &remote.tip(), Vec::new(), true),
        PullAnswer::Nothing,
        "our own export"
    );
    let theirs = merge_pr(
        &temp,
        &remote.mirror,
        "main",
        &[("pr.txt", "pr\n")],
        "from GitHub",
    );
    assert_eq!(
        answer(&remote, &base, &theirs, Vec::new(), true),
        PullAnswer::Work
    );
    assert_eq!(remote.main("root"), root);
}

#[test]
fn a_pull_moves_no_branch_itself() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    // Only the Worker's compare-and-swap on the list lands anything: the
    // container leaves the commit at its landing ref and `main` alone, so a
    // pull made from an index that has moved since lands nothing.
    let stale = remote.index();
    let repo = remote.store.paths().project_root.clone();
    std::fs::write(repo.join("ours.txt"), "ours\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "ours"]);
    manager(&remote.store).merge("main", "main").expect("merge");
    remote.publish();
    let root = remote.main("root");
    let theirs = remote.commit_on_mirror("pr", &[("pr.txt", "pr\n")], "from GitHub");
    let mut plan = remote.plan(&stale, &theirs);
    plan.instances = Vec::new();
    let PullAnswer::Landed { list, .. } = pull_mirror(&plan).expect("pull") else {
        panic!("it lands as far as the container goes");
    };
    assert_eq!(remote.main("root"), root, "root's `main` is where it was");
    assert_eq!(
        git(
            &remote.repos["root"],
            &[
                "rev-parse",
                &newgit_core::remote::landing_ref(&list["root"])
            ]
        ),
        list["root"],
        "its commit waits at a landing ref"
    );
}
