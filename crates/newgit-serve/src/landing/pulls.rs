//! Which open pull requests a batch of landings merges (#167, #172).
//!
//! The server decides, never a client: a pull request is merged by the
//! landing that leaves every commit of its head (its instance's latest push,
//! one per tracker in Git) on its branch, at or behind that tracker's new
//! head. So `newgit merge` of the author's instance, of a reviewer's `pr-N`,
//! or a mirror's pull all mark it, and naming another instance marks nothing.
//!
//! Worked out before the rows are written, outside the db lock (an ancestry
//! walk per tracker per open pull request on a moved branch), and written
//! with them, in the landing's own transaction, only if the pull request's
//! head is still the push that was checked: one pushed in between leaves it
//! open. Bookkeeping never fails a landing: what cannot be read is said and
//! left open.

use std::collections::{BTreeMap, BTreeSet};

use crate::App;
use crate::db::{self, List};
use crate::error::ApiResult;
use crate::readers::Readers;

use super::check::repo_in;
use super::entry::{Entry, Stamp};

/// A pull request a landing merges, as checked.
pub(super) struct Merged {
    pub(super) home: String,
    pub(super) number: i64,
    /// The push that was found on the branch: still its head, or it stays
    /// open.
    pub(super) head: String,
    /// Who landed it.
    pub(super) by: String,
}

/// One open pull request to check: its head's commits, and where each
/// tracker's history is.
struct Candidate {
    home: String,
    branch: String,
    number: i64,
    head: String,
    commits: Vec<(String, String, Option<String>)>,
}

/// The pull requests `entries` merge.
pub(super) fn merged_by(
    app: &App,
    entries: &[(Stamp, &Entry)],
    acting: &[BTreeSet<String>],
) -> Vec<Merged> {
    // Each branch as the batch leaves it, who set it last, and where each
    // tracker the batch moves between stores now is.
    let mut lists: BTreeMap<(String, String), (&List, &str)> = BTreeMap::new();
    let mut stores: BTreeMap<(String, String), String> = BTreeMap::new();
    for ((_, entry), here) in entries.iter().zip(acting) {
        for (home, change) in &entry.homes {
            if !here.contains(home) || change.removed {
                continue;
            }
            for (branch, list) in &change.branches {
                lists.insert((home.clone(), branch.clone()), (list, entry.by.as_str()));
            }
            for (tracker, step) in &change.stores {
                stores.insert((home.clone(), tracker.clone()), step.to.clone());
            }
        }
    }
    if lists.is_empty() {
        return Vec::new();
    }
    let candidates = match candidates(app, &lists, &stores) {
        Ok(candidates) => candidates,
        Err(error) => {
            eprintln!(
                "newgit serve: reading open pull requests for a landing: {}",
                error.message
            );
            return Vec::new();
        }
    };
    let mut readers = Readers::new();
    let data = &app.config.data;
    candidates
        .into_iter()
        .filter(|candidate| {
            let (list, _) = lists[&(candidate.home.clone(), candidate.branch.clone())];
            !candidate.commits.is_empty()
                && candidate.commits.iter().all(|(tracker, commit, store)| {
                    let Some(now) = list.get(tracker) else {
                        return false;
                    };
                    now == commit
                        || repo_in(data, &candidate.home, store.as_deref(), tracker)
                            .is_ok_and(|repo| readers.is_ancestor(&repo.dir(), commit, now))
                })
        })
        .map(|candidate| Merged {
            by: lists[&(candidate.home.clone(), candidate.branch.clone())]
                .1
                .to_owned(),
            home: candidate.home,
            number: candidate.number,
            head: candidate.head,
        })
        .collect()
}

/// Every open pull request on a branch in `lists`, with its head's commits:
/// read under the db lock, briefly.
fn candidates(
    app: &App,
    lists: &BTreeMap<(String, String), (&List, &str)>,
    stores: &BTreeMap<(String, String), String>,
) -> ApiResult<Vec<Candidate>> {
    let db = app.db.lock();
    let mut found = Vec::new();
    for (home, branch) in lists.keys() {
        for pull in db::open_pulls_on(&db, home, branch)? {
            if db::pull_instance(&db, home, &pull)?.is_none() {
                continue;
            }
            let Some(head) = db::latest_push(&db, home, &pull.instance)? else {
                continue;
            };
            let mut commits = Vec::new();
            for (tracker, commit) in crate::pulls::pushed_commits(&head) {
                let store = match stores.get(&(home.clone(), tracker.clone())) {
                    Some(store) => Some(store.clone()),
                    None => db::tracker_store(&db, home, &tracker)?,
                };
                commits.push((tracker, commit, store));
            }
            found.push(Candidate {
                home: home.clone(),
                branch: branch.clone(),
                number: pull.number,
                head: head["id"].as_str().unwrap_or_default().to_owned(),
                commits,
            });
        }
    }
    Ok(found)
}
