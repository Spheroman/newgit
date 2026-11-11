//! A merge whose branch moved again after it merged (landing.md, *Landing*
//! 3 and 8): landed on the new head without merging again when the two
//! changed different paths.
//!
//! - **What each side changed is a [`Touch`]**: per tracker whose commit
//!   moved, the paths of its own tree that changed (pins aside), and the
//!   directories that went. Read in process, skipping equal subtrees, so it
//!   costs what the change costs.
//! - **Two touches overlap** when, in a tracker both moved, one changed a
//!   path the other changed, or one inside a path the other changed, or one
//!   under a directory the other deleted or renamed away (so a file added
//!   under a directory another landing renamed follows the rename: that
//!   merge runs again, and Git's directory renames carry it), or when either
//!   added or dropped the tracker.
//! - **Otherwise the merge is re-made on the new head** ([`onto`]): every
//!   tracker only the merge moved keeps the merge's commit; one only the
//!   other landing moved keeps that; each tracker both moved (root, and
//!   parents along the mount chain) gets a new commit on the new head's,
//!   its tree the new head's with the merge's own paths and pins laid on.
//!   No `merge-tree`, no composing: O(depth) objects, written into the
//!   merger's landing for each tracker.
//! - **Nothing moves between trackers here**: each path is set from the
//!   same tracker's commit, so the audience rule's answer for the merge
//!   (`server_merge::carried`) holds for what this makes. The rules for
//!   what lands are still read again at the head it lands on.

use std::collections::{BTreeMap, BTreeSet};

use newgit_core::compose::{PINS_DIR, pins_in};
use newgit_core::objects::{GITLINK_MODE, Objects};

use crate::api::ProjectContext;
use crate::db::List;
use crate::error::ApiResult;
use crate::names::parent_of;
use crate::server_merge::Made;

/// What happened to one tracker's own content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Own {
    /// Its commit moved: the paths of its own tree that changed, and the
    /// directories that were there before and are not after.
    Paths {
        paths: BTreeSet<String>,
        gone: BTreeSet<String>,
    },
    /// New on this side, or dropped: overlaps any other change to it.
    Joined,
    Left,
}

/// What one side changed, from one index to another.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Touch(pub BTreeMap<String, Own>);

/// `path`'s directories, innermost last: `a/b/c` → `a`, `a/b`.
fn dirs_of(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(at, _)| &path[..at])
}

impl Touch {
    /// What moving `from` to `to` changes, read as merger `login` reads (its
    /// landings see the stores).
    pub fn between(
        project: &ProjectContext<'_>,
        login: &str,
        from: &List,
        to: &List,
    ) -> ApiResult<Self> {
        Self::against(project, login, from, to, None)
    }

    /// What moving `from` to `to` changes where it can meet `other`: its
    /// paths read only in the trackers `other` changed too, the rest known
    /// from the lists alone (they cannot overlap).
    pub fn meeting(
        project: &ProjectContext<'_>,
        login: &str,
        from: &List,
        to: &List,
        other: &Touch,
    ) -> ApiResult<Self> {
        Self::against(project, login, from, to, Some(other))
    }

    fn against(
        project: &ProjectContext<'_>,
        login: &str,
        from: &List,
        to: &List,
        other: Option<&Touch>,
    ) -> ApiResult<Self> {
        let mut touch = BTreeMap::new();
        let names: BTreeSet<&String> = from.keys().chain(to.keys()).collect();
        for tracker in names {
            let own = match (from.get(tracker), to.get(tracker)) {
                (Some(old), Some(new)) if old == new => continue,
                (None, _) => Own::Joined,
                (_, None) => Own::Left,
                (Some(_), Some(_)) if other.is_some_and(|other| !other.0.contains_key(tracker)) => {
                    Own::Paths {
                        paths: BTreeSet::new(),
                        gone: BTreeSet::new(),
                    }
                }
                (Some(old), Some(new)) => {
                    let mut objects = Objects::open(&project.tracker(tracker).view(login)?)?;
                    let old = objects.commit(old)?.tree;
                    let new = objects.commit(new)?.tree;
                    let pins = format!("{PINS_DIR}/");
                    let paths: BTreeSet<String> = objects
                        .changed_paths(Some(&old), Some(&new))?
                        .into_iter()
                        .filter(|path| !path.starts_with(&pins))
                        .collect();
                    let mut gone = BTreeSet::new();
                    let mut asked = BTreeSet::new();
                    for path in &paths {
                        for dir in dirs_of(path) {
                            if asked.insert(dir.to_owned())
                                && objects.subtree(&old, dir)?.is_some()
                                && objects.subtree(&new, dir)?.is_none()
                            {
                                gone.insert(dir.to_owned());
                            }
                        }
                    }
                    Own::Paths { paths, gone }
                }
            };
            touch.insert(tracker.clone(), own);
        }
        Ok(Self(touch))
    }

    /// The first tracker where this and `other` overlap, if any (see the
    /// module's docs).
    pub fn overlap(&self, other: &Self) -> Option<String> {
        for (tracker, mine) in &self.0 {
            let Some(theirs) = other.0.get(tracker) else {
                continue;
            };
            let (
                Own::Paths { paths, gone },
                Own::Paths {
                    paths: their_paths,
                    gone: their_gone,
                },
            ) = (mine, theirs)
            else {
                return Some(tracker.clone());
            };
            let clash =
                |paths: &BTreeSet<String>, gone: &BTreeSet<String>, theirs: &BTreeSet<String>| {
                    paths.iter().any(|path| {
                        theirs.contains(path)
                            || dirs_of(path).any(|dir| theirs.contains(dir) || gone.contains(dir))
                    })
                };
            if clash(paths, their_gone, their_paths) || clash(their_paths, gone, paths) {
                return Some(tracker.clone());
            }
        }
        None
    }
}

/// How deep `tracker` is mounted: root 0, its children 1, and so on.
fn depth(tracker: &str) -> usize {
    if tracker == "root" {
        0
    } else {
        tracker.matches("__").count() + 1
    }
}

/// The name `child` is pinned under in `parent`'s tree.
fn pin_name<'c>(parent: &str, child: &'c str) -> &'c str {
    if parent == "root" {
        child
    } else {
        child
            .strip_prefix(parent)
            .and_then(|rest| rest.strip_prefix("__"))
            .unwrap_or(child)
    }
}

/// The merge that made `target` from `head` (what `ours` says it changed),
/// re-made on `now`, which another landing moved `head` to and changed only
/// what `theirs` says, which does not overlap `ours`. Each tracker both
/// moved gets a new commit by `login`, first parent `now`'s commit: its
/// tree is `now`'s with `ours`' own paths set as `target` has them and each
/// pin `target` moved set to what lands for that child. Its other parents
/// are the merge commit's (the merger's commit), or the merger's commit
/// itself when the merge kept it. Each waits at its landing ref in `made`.
#[allow(clippy::too_many_arguments, reason = "each is one thing a merge is")]
pub fn onto(
    project: &ProjectContext<'_>,
    login: &str,
    branch: &str,
    head: &List,
    target: &List,
    now: &List,
    ours: &Touch,
    theirs: &Touch,
    made: &mut Made,
) -> ApiResult<List> {
    let mut landing = now.clone();
    for tracker in ours.0.keys() {
        if theirs.0.contains_key(tracker) {
            continue;
        }
        match target.get(tracker) {
            Some(commit) => landing.insert(tracker.clone(), commit.clone()),
            None => landing.remove(tracker),
        };
    }
    let mut both: Vec<&String> = ours
        .0
        .keys()
        .filter(|tracker| theirs.0.contains_key(*tracker))
        .collect();
    both.sort_by_key(|tracker| std::cmp::Reverse(depth(tracker)));
    let stamp = chrono::Utc::now().timestamp();
    let by = format!("{login} <{login}@users.noreply.github.com> {stamp} +0000");
    let message = format!("Merge `{login}` into {branch}\n");
    for tracker in both {
        let Some(Own::Paths { paths, .. }) = ours.0.get(tracker) else {
            continue;
        };
        let dir = project.tracker(tracker).open_landing(login)?;
        let mut objects = Objects::open(&dir)?;
        let at_now = objects.commit(&now[tracker])?;
        let at_head = objects.commit(&head[tracker])?;
        let at_target = objects.commit(&target[tracker])?;
        let mut tree = at_now.tree.clone();
        for path in paths {
            let entry = objects
                .entry_at(&at_target.tree, path)?
                .filter(|entry| !entry.is_tree())
                .map(|entry| (entry.mode, entry.id));
            tree = objects.set_path(&tree, path, entry)?;
        }
        // Each pin the merge moved, to what lands for that child now.
        let (before, after) = (
            pins_in(&mut objects, &at_head.tree)?,
            pins_in(&mut objects, &at_target.tree)?,
        );
        let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
        for name in names {
            if before.get(name) == after.get(name) {
                continue;
            }
            let child = landing
                .keys()
                .chain(target.keys())
                .find(|child| {
                    parent_of(child).unwrap_or("root") == tracker
                        && *child != tracker
                        && pin_name(tracker, child) == name
                })
                .cloned();
            let pin = child
                .and_then(|child| landing.get(&child).cloned())
                .map(|commit| (GITLINK_MODE.to_vec(), commit));
            tree = objects.set_path(&tree, &format!("{PINS_DIR}/{name}"), pin)?;
        }
        let mut parents = vec![now[tracker].clone()];
        if made.has(tracker, &target[tracker]) {
            parents.extend(at_target.parents.iter().skip(1).cloned());
        } else {
            parents.push(target[tracker].clone());
        }
        let commit = objects.make_commit(
            &tree,
            &parents,
            by.as_bytes(),
            by.as_bytes(),
            b"",
            message.as_bytes(),
        );
        objects.flush()?;
        made.wait(&dir, tracker, &commit)?;
        landing.insert(tracker.clone(), commit);
    }
    Ok(landing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(paths: &[&str], gone: &[&str]) -> Own {
        Own::Paths {
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            gone: gone.iter().map(|dir| (*dir).to_owned()).collect(),
        }
    }

    fn touch(entries: &[(&str, Own)]) -> Touch {
        Touch(
            entries
                .iter()
                .map(|(tracker, own)| ((*tracker).to_owned(), own.clone()))
                .collect(),
        )
    }

    #[test]
    fn different_trackers_never_overlap() {
        let ours = touch(&[("root", paths(&[], &[])), ("a", paths(&["x"], &[]))]);
        let theirs = touch(&[("root", paths(&[], &[])), ("b", paths(&["x"], &[]))]);
        assert_eq!(ours.overlap(&theirs), None);
    }

    #[test]
    fn the_same_path_or_one_inside_another_overlaps() {
        let ours = touch(&[("root", paths(&["docs/a.md"], &[]))]);
        assert_eq!(
            ours.overlap(&touch(&[("root", paths(&["docs/a.md"], &[]))])),
            Some("root".into())
        );
        assert_eq!(
            ours.overlap(&touch(&[("root", paths(&["docs"], &[]))])),
            Some("root".into())
        );
        assert_eq!(
            ours.overlap(&touch(&[("root", paths(&["docs/b.md"], &[]))])),
            None
        );
    }

    #[test]
    fn a_directory_gone_overlaps_everything_under_it() {
        let ours = touch(&[("root", paths(&["docs/new.md"], &[]))]);
        let theirs = touch(&[(
            "root",
            paths(&["docs/guide.md", "guide/guide.md"], &["docs"]),
        )]);
        assert_eq!(ours.overlap(&theirs), Some("root".into()));
        assert_eq!(theirs.overlap(&ours), Some("root".into()));
    }

    #[test]
    fn a_tracker_added_or_dropped_overlaps() {
        let ours = touch(&[("a", Own::Joined)]);
        let theirs = touch(&[("a", paths(&[], &[]))]);
        assert_eq!(ours.overlap(&theirs), Some("a".into()));
    }

    #[test]
    fn pin_names_drop_the_parent() {
        assert_eq!(pin_name("root", "blog"), "blog");
        assert_eq!(pin_name("a", "a__b"), "b");
        assert_eq!(depth("a__b"), 2);
    }
}
