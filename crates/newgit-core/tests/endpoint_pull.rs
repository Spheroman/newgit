//! `newgit endpoint-pull`: a mirror's branch moved, and the endpoint's
//! container brings the commit into `main`, as `newgit pull --remote` does
//! for a target without `root`, against bare repositories the way the remote
//! holds trackers. Commits that came from here change nothing; the rest
//! become root commits with the withheld trackers' pins carried over, and go
//! back out to the mirror as the commits it already has.

mod imports;

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use imports::*;
use newgit_core::compose;
use newgit_core::endpoint::{Mirror, PullAnswer, PullPlan, Source, pull_mirror};
use newgit_core::manager::RemoteExportOptions;
use newgit_core::store::MetadataStore;

struct Remote {
    store: MetadataStore,
    temp: Utf8PathBuf,
    mirror: Utf8PathBuf,
    repos: BTreeMap<String, Utf8PathBuf>,
}

impl Remote {
    /// Each tracker's published `main`, into its own bare repository.
    fn publish(&self) {
        let tracker_store = self.store.paths().tracker_store.clone();
        for (tracker, repo) in &self.repos {
            git(
                &self.temp,
                &[
                    &format!("--git-dir={tracker_store}"),
                    "push",
                    "-q",
                    "--force",
                    repo.as_str(),
                    &format!("refs/trackers/{tracker}/main:refs/heads/main"),
                ],
            );
        }
    }

    fn main(&self, tracker: &str) -> String {
        git(&self.repos[tracker], &["rev-parse", "refs/heads/main"])
    }

    /// `main`'s index, as the Worker holds it.
    fn index(&self) -> BTreeMap<String, String> {
        let tracker_store = self.store.paths().tracker_store.clone();
        let root = store_ref(&self.store, "refs/trackers/root/main").expect("root");
        compose::index(&tracker_store, "root", &root).expect("index")
    }

    fn plan(&self, list: &BTreeMap<String, String>, commit: &str) -> PullPlan {
        PullPlan {
            dir: self.temp.join("work"),
            repos: self
                .repos
                .iter()
                .map(|(tracker, repo)| {
                    (
                        tracker.clone(),
                        Source {
                            url: repo.to_string(),
                            auth: "Authorization: Basic eDpwYXNz".to_owned(),
                        },
                    )
                })
                .collect(),
            list: list.clone(),
            writable: vec!["root".to_owned(), "access".to_owned()],
            target: "github".to_owned(),
            mirror: Mirror {
                url: self.mirror.to_string(),
                branch: "main".to_owned(),
                commit: commit.to_owned(),
            },
        }
    }

    fn tip(&self) -> String {
        git(&self.mirror, &["rev-parse", "main"])
    }

    fn landed(&self, list: &BTreeMap<String, String>, commit: &str) -> BTreeMap<String, String> {
        match pull_mirror(&self.plan(list, commit)).expect("pull") {
            PullAnswer::Landed { list, .. } => list,
            other => panic!("{other:?}"),
        }
    }

    fn refused(&self, list: &BTreeMap<String, String>, commit: &str) -> String {
        match pull_mirror(&self.plan(list, commit)).expect("pull") {
            PullAnswer::Refused { why } => why,
            other => panic!("{other:?}"),
        }
    }

    /// Commit `files` on the mirror's `main`, pushed directly.
    fn commit_on_mirror(&self, name: &str, files: &[(&str, &str)], message: &str) -> String {
        let clone = self.temp.join(name);
        git(
            &self.temp,
            &[
                "clone",
                "-q",
                "-b",
                "main",
                self.mirror.as_str(),
                clone.as_str(),
            ],
        );
        git(&clone, &["config", "commit.gpgSign", "false"]);
        let commit = commit_as(&clone, "pat", 2_000_000_000, files, message);
        git(&clone, &["push", "-q", "origin", "main"]);
        commit
    }
}

/// A project with `t3code` withheld from a mirror it exported to, and every
/// tracker in a bare repository of its own.
fn remote(temp: &Utf8Path) -> Remote {
    let upstream = upstream(temp);
    let store = project(temp);
    let mirror = temp.join("mirror.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    import(&store, &upstream);
    define_target(
        &store,
        "github",
        &format!("url = \"{mirror}\"\ntrackers = [\"root\", \"access\"]\n"),
    );
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export target"]);
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("github", RemoteExportOptions::default())
        .expect("export");
    let mut repos = BTreeMap::new();
    for tracker in ["root", "access", "t3code"] {
        let bare = temp.join(format!("{tracker}.git"));
        git(temp, &["init", "-q", "--bare", "-b", "main", bare.as_str()]);
        repos.insert(tracker.to_owned(), bare);
    }
    let remote = Remote {
        store,
        temp: temp.to_path_buf(),
        mirror,
        repos,
    };
    remote.publish();
    remote
}

#[test]
fn a_merged_pr_lands_with_withheld_pins_carried_over() {
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

    let list = remote.landed(&base, &theirs);
    assert_eq!(remote.main("root"), list["root"], "root's ref moved");
    assert_ne!(list["root"], base["root"]);
    assert_eq!(list["t3code"], base["t3code"], "t3code's pin carried over");
    let root = &remote.repos["root"];
    assert_eq!(
        git(root, &["rev-parse", "main:.newgit/pins/t3code"]),
        base["t3code"],
    );
    assert_eq!(git(root, &["show", "main:pr.txt"]), "pr");

    // Asked again, with the index the Worker then wrote: nothing, because the
    // mirror's commits are now what composing `main` gives.
    assert_eq!(
        pull_mirror(&remote.plan(&list, &theirs)).expect("again"),
        PullAnswer::Nothing
    );
}

#[test]
fn an_event_for_commits_that_came_from_here_changes_nothing() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let root = remote.main("root");
    let tip = remote.tip();
    assert_eq!(
        pull_mirror(&remote.plan(&base, &tip)).expect("pull"),
        PullAnswer::Nothing
    );
    assert_eq!(remote.main("root"), root);
    // An older commit of ours is on the mirror too.
    let older = git(&remote.mirror, &["rev-parse", "main^"]);
    assert_eq!(
        pull_mirror(&remote.plan(&base, &older)).expect("pull"),
        PullAnswer::Nothing
    );
}

/// `commit`'s object rewritten with a signature header: GitHub signs what it
/// merges, and a verifier is not what this is testing.
fn signed(mirror: &Utf8Path, commit: &str) -> String {
    let raw = git(mirror, &["cat-file", "commit", commit]);
    let (head, body) = raw.split_once("\n\n").expect("a commit");
    let signature = "gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----";
    let object = format!("{head}\n{signature}\n\n{body}\n");
    let path = mirror.join("signed-object");
    std::fs::write(&path, object).expect("write");
    let id = git(
        mirror,
        &["hash-object", "-t", "commit", "-w", path.as_str()],
    );
    git(mirror, &["update-ref", "refs/heads/main", &id]);
    id
}

#[test]
fn a_signed_commit_goes_back_out_as_itself() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let commit = remote.commit_on_mirror("pr", &[("pr.txt", "pr\n")], "from GitHub");
    let theirs = signed(&remote.mirror, &commit);

    let list = remote.landed(&base, &theirs);
    assert_eq!(
        pull_mirror(&remote.plan(&list, &theirs)).expect("again"),
        PullAnswer::Nothing,
        "composing for the mirror gives its signed commit back, id and all"
    );
}

#[test]
fn a_commit_touching_a_withheld_mount_is_refused_and_moves_nothing() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let root = remote.main("root");
    let theirs = remote.commit_on_mirror("pr", &[("apps/t3code/x.ts", "x\n")], "sneaky");
    let why = remote.refused(&base, &theirs);
    assert!(why.contains("apps/t3code"), "{why}");
    assert_eq!(remote.main("root"), root);
}

#[test]
fn a_file_owned_by_a_tracker_outside_git_is_refused() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let repo = remote.store.paths().project_root.clone();
    std::fs::write(
        repo.join(".newgit/trackers/env.toml"),
        "audience = \"user\"\nstorage = \"local\"\npaths = [\".env\"]\n",
    )
    .expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "env"]);
    manager(&remote.store).merge("main", "main").expect("merge");
    manager(&remote.store)
        .export_to_remote("github", RemoteExportOptions::default())
        .expect("export");
    remote.publish();
    let base = remote.index();
    let root = remote.main("root");
    let theirs = remote.commit_on_mirror("pr", &[(".env", "SECRET=1\n")], "oops");
    let why = remote.refused(&base, &theirs);
    assert!(why.contains("`.env`"), "{why}");
    assert_eq!(remote.main("root"), root);
}

#[test]
fn a_tracker_the_pusher_does_not_maintain_is_refused() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let root = remote.main("root");
    let theirs = remote.commit_on_mirror(
        "pr",
        &[(".newgit/trackers/access-note.txt", "note\n")],
        "edits access",
    );
    let mut plan = remote.plan(&base, &theirs);
    plan.writable = vec!["root".to_owned()];
    match pull_mirror(&plan).expect("pull") {
        PullAnswer::Refused { why } => assert!(why.contains("`access`"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(remote.main("root"), root);
}

#[test]
fn main_having_moved_since_the_export_is_a_merge() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let repo = remote.store.paths().project_root.clone();
    std::fs::write(repo.join("ours.txt"), "ours\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "ours, not exported yet"]);
    manager(&remote.store).merge("main", "main").expect("merge");
    remote.publish();
    let base = remote.index();
    let theirs = remote.commit_on_mirror("pr", &[("pr.txt", "pr\n")], "from GitHub");

    let list = remote.landed(&base, &theirs);
    let root = &remote.repos["root"];
    assert_eq!(git(root, &["show", "main:pr.txt"]), "pr");
    assert_eq!(git(root, &["show", "main:ours.txt"]), "ours");
    assert_eq!(
        git(root, &["rev-parse", "main^1"]),
        base["root"],
        "a merge on top of `main`"
    );
    // A redelivery finds it landed.
    assert_eq!(
        pull_mirror(&remote.plan(&list, &theirs)).expect("again"),
        PullAnswer::Nothing
    );
}

#[test]
fn a_conflict_with_main_is_refused() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let repo = remote.store.paths().project_root.clone();
    std::fs::write(repo.join("README.md"), "ours\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "ours"]);
    manager(&remote.store).merge("main", "main").expect("merge");
    remote.publish();
    let base = remote.index();
    let root = remote.main("root");
    let theirs = remote.commit_on_mirror("pr", &[("README.md", "theirs\n")], "theirs");
    let why = remote.refused(&base, &theirs);
    assert!(why.contains("README.md"), "{why}");
    assert_eq!(remote.main("root"), root);
}

#[test]
fn a_run_that_stopped_after_root_moved_finishes_with_the_same_commits() {
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
    let first = remote.landed(&base, &theirs);
    // The index was never written, so the Worker asks again from `base`.
    let again = remote.landed(&base, &theirs);
    assert_eq!(first, again);
    assert_eq!(remote.main("root"), first["root"]);
}

#[test]
fn a_commit_the_branch_no_longer_holds_is_refused() {
    let (_guard, temp) = tempdir();
    let remote = remote(&temp);
    let base = remote.index();
    let why = remote.refused(&base, &"1".repeat(40));
    assert!(why.contains("no longer holds"), "{why}");
}
