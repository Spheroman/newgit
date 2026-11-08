//! A project whose mirror withholds a tracker, with every tracker in a bare
//! repository of its own: what `newgit endpoint-pull` runs against.

#![allow(dead_code)]

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose;
use newgit_core::endpoint::{Mirror, PullAnswer, PullPlan, Source, pull_mirror};
use newgit_core::manager::RemoteExportOptions;
use newgit_core::store::MetadataStore;

use crate::imports::*;

pub struct Remote {
    pub store: MetadataStore,
    pub temp: Utf8PathBuf,
    pub mirror: Utf8PathBuf,
    pub repos: BTreeMap<String, Utf8PathBuf>,
}

impl Remote {
    /// Each tracker's published `main`, into its own bare repository.
    pub fn publish(&self) {
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

    pub fn main(&self, tracker: &str) -> String {
        git(&self.repos[tracker], &["rev-parse", "refs/heads/main"])
    }

    /// `main`'s index, as the Worker holds it.
    pub fn index(&self) -> BTreeMap<String, String> {
        let tracker_store = self.store.paths().tracker_store.clone();
        let root = store_ref(&self.store, "refs/trackers/root/main").expect("root");
        compose::index(&tracker_store, "root", &root).expect("index")
    }

    pub fn plan(&self, list: &BTreeMap<String, String>, commit: &str) -> PullPlan {
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
            instances: Vec::new(),
            peek: false,
            tag: None,
        }
    }

    pub fn tip(&self) -> String {
        git(&self.mirror, &["rev-parse", "main"])
    }

    pub fn landed(
        &self,
        list: &BTreeMap<String, String>,
        commit: &str,
    ) -> BTreeMap<String, String> {
        let answer = pull_mirror(&self.plan(list, commit)).expect("pull");
        self.follow(&answer);
        match answer {
            PullAnswer::Landed { list, .. } => list,
            other => panic!("{other:?}"),
        }
    }

    /// What the Worker does once a pull's list lands: root's `main` follows
    /// it, each move is made, and the landing refs go. The container moves
    /// no branch itself.
    pub fn follow(&self, answer: &PullAnswer) {
        let PullAnswer::Landed { list, moves, .. } = answer else {
            return;
        };
        let root = &self.repos["root"];
        assert_eq!(
            git(
                root,
                &[
                    "rev-parse",
                    &newgit_core::remote::landing_ref(&list["root"])
                ]
            ),
            list["root"],
            "the root commit waits at its landing ref"
        );
        git(root, &["update-ref", "refs/heads/main", &list["root"]]);
        git(
            root,
            &[
                "update-ref",
                "-d",
                &newgit_core::remote::landing_ref(&list["root"]),
            ],
        );
        for step in moves {
            let repo = &self.repos[&step.tracker];
            let old = step.old.clone().unwrap_or_else(|| "0".repeat(40));
            git(
                repo,
                &[
                    "update-ref",
                    &format!("refs/heads/{}", step.branch),
                    &step.new,
                    &old,
                ],
            );
            git(
                repo,
                &[
                    "update-ref",
                    "-d",
                    &newgit_core::remote::landing_ref(&step.new),
                ],
            );
        }
    }

    pub fn refused(&self, list: &BTreeMap<String, String>, commit: &str) -> String {
        match pull_mirror(&self.plan(list, commit)).expect("pull") {
            PullAnswer::Refused { why } => why,
            other => panic!("{other:?}"),
        }
    }

    /// Commit `files` on the mirror's `main`, pushed directly.
    pub fn commit_on_mirror(&self, name: &str, files: &[(&str, &str)], message: &str) -> String {
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
pub fn remote(temp: &Utf8Path) -> Remote {
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
