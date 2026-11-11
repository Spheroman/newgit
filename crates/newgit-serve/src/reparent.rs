//! A merge whose branch moved again after it merged (landing.md, *Landing*
//! 3 and 8): landed on the new head without merging again when the two
//! changed different paths.
//!
//! - **What each side changed is a [`Touch`]**: per tracker whose commit
//!   moved, the paths of its own tree that changed (pins aside), and the
//!   directories that went. Read in process, skipping equal subtrees, so it
//!   costs what the change costs. Paths are the bytes Git stores: a name
//!   need not be UTF-8, and the re-make sets each path by the same bytes it
//!   read.
//! - **Two touches overlap** when, in a tracker both moved, one changed a
//!   path the other changed, or one inside a path the other changed, or one
//!   under a directory the other deleted or renamed away (so a file added
//!   under a directory another landing renamed follows the rename: that
//!   merge runs again, and Git's directory renames carry it).
//! - **Some changes reshape the composed tree itself**, and only Git's merge
//!   of the composed trees can follow them: a tracker added or dropped, a
//!   tracker whose own content went entirely (its directory in the view is
//!   gone, so Git may follow it elsewhere as a directory rename, and so may
//!   the directories above it), a definition changed (a mount moved, so a
//!   whole directory of the view did), and pins the re-make cannot write
//!   exactly as the merge has them. A touch with any of these is
//!   [`Touch::whole`]: it overlaps every other touch.
//! - **Otherwise the merge is re-made on the new head** ([`onto`]): every
//!   tracker only the merge moved keeps the merge's commit; one only the
//!   other landing moved keeps that; each tracker both moved (root, and
//!   parents along the mount chain) gets a new commit on the new head's,
//!   its tree the new head's with the merge's own paths and pins laid on.
//!   No `merge-tree`, no composing: O(depth) objects, written into the
//!   merger's landing for each tracker. `crates/newgit-serve/tests/reparent_differential.rs`
//!   holds this to Git's merge of the composed trees.
//! - **Nothing moves between trackers here**: each path is set from the
//!   same tracker's commit, so the audience rule's answer for the merge
//!   (`server_merge::carried`) holds for what this makes. The rules for
//!   what lands are still read again at the head it lands on.

use std::collections::{BTreeMap, BTreeSet};

use newgit_core::compose::PINS_DIR;
use newgit_core::objects::{Entry, GITLINK_MODE, Objects};
use newgit_core::tracker::{ACCESS_TRACKER, DEFINITIONS_DIR};

use crate::api::ProjectContext;
use crate::db::List;
use crate::error::{ApiResult, HttpError};
use crate::server_merge::Made;

/// What happened to one tracker's own content: its commit moved, and these
/// paths of its own tree changed, and these directories were there before
/// and are not after. Both as the bytes Git stores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Own {
    pub paths: BTreeSet<Vec<u8>>,
    pub gone: BTreeSet<Vec<u8>>,
}

/// What one side changed, from one index to another.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Touch {
    pub trackers: BTreeMap<String, Own>,
    /// The tracker whose change reshapes the composed tree (see the
    /// module's docs), if any: then this overlaps every other touch, and
    /// only Git's merge lands it.
    pub whole: Option<String>,
}

/// `path`'s directories, innermost last: `a/b/c` → `a`, `a/b`.
fn dirs_of(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(move |(at, _)| &path[..at])
}

/// The child `tracker` pins under `name`.
fn child_of(tracker: &str, name: &str) -> String {
    if tracker == "root" {
        name.to_owned()
    } else {
        format!("{tracker}__{name}")
    }
}

/// Every entry directly under `tree`'s `.newgit/pins/`, by name as stored.
fn pin_entries(objects: &mut Objects, tree: &str) -> ApiResult<BTreeMap<Vec<u8>, Entry>> {
    let Some(pins) = objects.subtree(tree, PINS_DIR)? else {
        return Ok(BTreeMap::new());
    };
    Ok(objects
        .tree(&pins)?
        .into_iter()
        .map(|entry| (entry.name.clone(), entry))
        .collect())
}

/// Whether `tree` holds anything of its own tracker's: any entry but
/// `.newgit/pins`.
fn holds_own(objects: &mut Objects, tree: &str) -> ApiResult<bool> {
    for entry in objects.tree(tree)? {
        if entry.name != b".newgit" || !entry.is_tree() {
            return Ok(true);
        }
        if objects
            .tree(&entry.id)?
            .iter()
            .any(|inner| inner.name != b"pins" || !inner.is_tree())
        {
            return Ok(true);
        }
    }
    Ok(false)
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
        Self::read(&mut viewer(project, login), from, to, None)
    }

    /// What moving `from` to `to` changes where it can meet `other`: its
    /// paths read only in the trackers `other` changed too, the rest known
    /// from the lists alone (they cannot overlap). Whether it reshapes the
    /// composed tree is read for every tracker: that meets anything.
    pub fn meeting(
        project: &ProjectContext<'_>,
        login: &str,
        from: &List,
        to: &List,
        other: &Touch,
    ) -> ApiResult<Self> {
        Self::read(&mut viewer(project, login), from, to, Some(other))
    }

    /// What moving `from` to `to` changes, each tracker's objects read
    /// through `open`; where it can meet `other`, when given (see
    /// [`Touch::meeting`]).
    pub fn read(
        open: &mut dyn FnMut(&str) -> ApiResult<Objects>,
        from: &List,
        to: &List,
        other: Option<&Touch>,
    ) -> ApiResult<Self> {
        let mut touch = Self::default();
        let names: BTreeSet<&String> = from.keys().chain(to.keys()).collect();
        for tracker in names {
            let (old, new) = match (from.get(tracker), to.get(tracker)) {
                (Some(old), Some(new)) if old == new => continue,
                (Some(old), Some(new)) => (old, new),
                // Added or dropped: its directory in the view came or went.
                _ => {
                    touch.whole.get_or_insert_with(|| tracker.clone());
                    touch.trackers.insert(tracker.clone(), Own::default());
                    continue;
                }
            };
            // `access` holds the definitions of root's children: where each
            // is mounted.
            if tracker == ACCESS_TRACKER {
                touch.whole.get_or_insert_with(|| tracker.clone());
            }
            let mut objects = open(tracker)?;
            let old = objects.commit(old)?.tree;
            let new = objects.commit(new)?.tree;
            let reshaped = objects.subtree(&old, DEFINITIONS_DIR)?
                != objects.subtree(&new, DEFINITIONS_DIR)?
                || (holds_own(&mut objects, &old)? && !holds_own(&mut objects, &new)?);
            if reshaped {
                touch.whole.get_or_insert_with(|| tracker.clone());
            }
            if other.is_some_and(|other| !other.trackers.contains_key(tracker)) {
                touch.trackers.insert(tracker.clone(), Own::default());
                continue;
            }
            let pins = format!("{PINS_DIR}/");
            let mut own = Own::default();
            for path in objects.changed_paths(Some(&old), Some(&new))? {
                if path == PINS_DIR.as_bytes() {
                    touch.whole.get_or_insert_with(|| tracker.clone());
                } else if !path.starts_with(pins.as_bytes()) {
                    own.paths.insert(path);
                }
            }
            if !pins_exact(&mut objects, tracker, &old, &new, to)? {
                touch.whole.get_or_insert_with(|| tracker.clone());
            }
            let mut asked = BTreeSet::new();
            for path in &own.paths {
                for dir in dirs_of(path) {
                    if asked.insert(dir.to_vec())
                        && objects.subtree(&old, dir)?.is_some()
                        && objects.subtree(&new, dir)?.is_none()
                    {
                        own.gone.insert(dir.to_vec());
                    }
                }
            }
            touch.trackers.insert(tracker.clone(), own);
        }
        Ok(touch)
    }

    /// The first tracker where this and `other` overlap, if any (see the
    /// module's docs).
    pub fn overlap(&self, other: &Self) -> Option<String> {
        if let Some(whole) = self.whole.as_ref().or(other.whole.as_ref()) {
            return Some(whole.clone());
        }
        for (tracker, mine) in &self.trackers {
            let Some(theirs) = other.trackers.get(tracker) else {
                continue;
            };
            let clash = |paths: &BTreeSet<Vec<u8>>,
                         gone: &BTreeSet<Vec<u8>>,
                         theirs: &BTreeSet<Vec<u8>>| {
                paths.iter().any(|path| {
                    theirs.contains(path)
                        || dirs_of(path).any(|dir| theirs.contains(dir) || gone.contains(dir))
                })
            };
            if clash(&mine.paths, &theirs.gone, &theirs.paths)
                || clash(&theirs.paths, &mine.gone, &mine.paths)
            {
                return Some(tracker.clone());
            }
        }
        None
    }

    /// This touch widened by `more`: what both changed.
    pub fn widen(&mut self, more: &Touch) {
        if self.whole.is_none() {
            self.whole.clone_from(&more.whole);
        }
        for (tracker, own) in &more.trackers {
            let entry = self.trackers.entry(tracker.clone()).or_default();
            entry.paths.extend(own.paths.iter().cloned());
            entry.gone.extend(own.gone.iter().cloned());
        }
    }
}

/// Whether every pin `new` changed from `old` is one [`onto`] writes back
/// exactly: a gitlink directly under `.newgit/pins/`, named in UTF-8 for a
/// child `to` lists at that very commit. Anything else (a file there, a pin
/// for no listed child, a pin removed) only Git's merge may judge.
fn pins_exact(
    objects: &mut Objects,
    tracker: &str,
    old: &str,
    new: &str,
    to: &List,
) -> ApiResult<bool> {
    let (before, after) = (pin_entries(objects, old)?, pin_entries(objects, new)?);
    let names: BTreeSet<&Vec<u8>> = before.keys().chain(after.keys()).collect();
    for name in names {
        if before.get(name) == after.get(name) {
            continue;
        }
        let Some(entry) = after.get(name) else {
            return Ok(false);
        };
        let Ok(name) = std::str::from_utf8(name) else {
            return Ok(false);
        };
        if entry.mode != GITLINK_MODE || to.get(&child_of(tracker, name)) != Some(&entry.id) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// How deep `tracker` is mounted: root 0, its children 1, and so on.
fn depth(tracker: &str) -> usize {
    if tracker == "root" {
        0
    } else {
        tracker.matches("__").count() + 1
    }
}

/// How `login` reads each tracker's objects: its landings see the stores.
fn viewer<'p>(
    project: &'p ProjectContext<'_>,
    login: &'p str,
) -> impl FnMut(&str) -> ApiResult<Objects> + 'p {
    move |tracker| Ok(Objects::open(&project.tracker(tracker).view(login)?)?)
}

/// Where [`remake`] writes the commits it makes.
pub trait Landings {
    /// The objects of the repository a commit re-made for `tracker` is
    /// written to; it reads every commit of that tracker's the merge names.
    fn open(&mut self, tracker: &str) -> ApiResult<Objects>;
    /// `commit`, flushed into `objects` (from [`Landings::open`]), kept
    /// until it lands or is given up.
    fn wait(&mut self, objects: &Objects, tracker: &str, commit: &str) -> ApiResult<()>;
    /// Whether this merge made `commit` for `tracker` (a merge commit,
    /// whose second parent is the merger's).
    fn made(&self, tracker: &str, commit: &str) -> bool;
}

/// The merger's landings, each commit made kept in `made`.
struct Merger<'p, 'm> {
    project: &'p ProjectContext<'p>,
    login: &'p str,
    made: &'m mut Made,
}

impl Landings for Merger<'_, '_> {
    fn open(&mut self, tracker: &str) -> ApiResult<Objects> {
        let dir = self.project.tracker(tracker).open_landing(self.login)?;
        Ok(Objects::open(&dir)?)
    }

    fn wait(&mut self, objects: &Objects, tracker: &str, commit: &str) -> ApiResult<()> {
        self.made.wait(objects.repo(), tracker, commit)
    }

    fn made(&self, tracker: &str, commit: &str) -> bool {
        self.made.has(tracker, commit)
    }
}

/// [`remake`], into the merger's landings, each commit kept in `made`.
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
    let stamp = chrono::Utc::now().timestamp();
    remake(
        &mut Merger {
            project,
            login,
            made,
        },
        login,
        branch,
        stamp,
        [head, target, now],
        [ours, theirs],
    )
}

/// The merge that made `target` from `head` (what `ours` says it changed),
/// re-made on `now`, which another landing moved `head` to and changed only
/// what `theirs` says, which does not overlap `ours`. Each tracker both
/// moved gets a new commit by `login`, first parent `now`'s commit: its
/// tree is `now`'s with `ours`' own paths set as `target` has them and each
/// pin `target` moved set to what lands for that child. Its other parents
/// are the merge commit's (the merger's commit), or the merger's commit
/// itself when the merge kept it. Each waits at its landing ref in `made`.
pub fn remake(
    landings: &mut dyn Landings,
    login: &str,
    branch: &str,
    stamp: i64,
    [head, target, now]: [&List; 3],
    [ours, theirs]: [&Touch; 2],
) -> ApiResult<List> {
    let mut landing = now.clone();
    for tracker in ours.trackers.keys() {
        if theirs.trackers.contains_key(tracker) {
            continue;
        }
        match target.get(tracker) {
            Some(commit) => landing.insert(tracker.clone(), commit.clone()),
            None => landing.remove(tracker),
        };
    }
    let mut both: Vec<&String> = ours
        .trackers
        .keys()
        .filter(|tracker| theirs.trackers.contains_key(*tracker))
        .collect();
    both.sort_by_key(|tracker| std::cmp::Reverse(depth(tracker)));
    let by = format!("{login} <{login}@users.noreply.github.com> {stamp} +0000");
    let message = format!("Merge `{login}` into {branch}\n");
    for tracker in both {
        let paths = &ours.trackers[tracker].paths;
        let mut objects = landings.open(tracker)?;
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
        // Each pin the merge moved, to what lands for that child now. The
        // touch held each to a gitlink for a listed child (`pins_exact`).
        let (before, after) = (
            pin_entries(&mut objects, &at_head.tree)?,
            pin_entries(&mut objects, &at_target.tree)?,
        );
        for (name, entry) in &after {
            if before.get(name) == Some(entry) {
                continue;
            }
            let child = std::str::from_utf8(name)
                .ok()
                .map(|name| child_of(tracker, name))
                .filter(|child| target.get(child) == Some(&entry.id))
                .ok_or_else(|| {
                    HttpError::internal(format!(
                        "`{tracker}` pins a child the merge does not list: it is not re-made"
                    ))
                })?;
            let pin = landing
                .get(&child)
                .map(|commit| (GITLINK_MODE.to_vec(), commit.clone()));
            let mut at = format!("{PINS_DIR}/").into_bytes();
            at.extend_from_slice(name);
            tree = objects.set_path(&tree, &at, pin)?;
        }
        if before.keys().any(|name| !after.contains_key(name)) {
            return Err(HttpError::internal(format!(
                "`{tracker}`'s merge drops a pin: it is not re-made"
            )));
        }
        let mut parents = vec![now[tracker].clone()];
        if landings.made(tracker, &target[tracker]) {
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
        landings.wait(&objects, tracker, &commit)?;
        landing.insert(tracker.clone(), commit);
    }
    Ok(landing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(paths: &[&str], gone: &[&str]) -> Own {
        Own {
            paths: paths.iter().map(|path| path.as_bytes().to_vec()).collect(),
            gone: gone.iter().map(|dir| dir.as_bytes().to_vec()).collect(),
        }
    }

    fn touch(entries: &[(&str, Own)]) -> Touch {
        Touch {
            trackers: entries
                .iter()
                .map(|(tracker, own)| ((*tracker).to_owned(), own.clone()))
                .collect(),
            whole: None,
        }
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
    fn names_are_bytes_not_their_lossy_text() {
        let ours = Touch {
            trackers: [(
                "root".to_owned(),
                Own {
                    paths: [b"caf\xe9.md".to_vec()].into(),
                    gone: BTreeSet::new(),
                },
            )]
            .into(),
            whole: None,
        };
        let theirs = Touch {
            trackers: [(
                "root".to_owned(),
                Own {
                    paths: [b"caf\xff.md".to_vec()].into(),
                    gone: BTreeSet::new(),
                },
            )]
            .into(),
            whole: None,
        };
        assert_eq!(ours.overlap(&theirs), None);
        assert_eq!(ours.overlap(&ours), Some("root".into()));
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
    fn a_reshaped_view_overlaps_everything() {
        let ours = touch(&[("root", paths(&["a.md"], &[]))]);
        let mut theirs = touch(&[("blog", paths(&["post.md"], &[]))]);
        assert_eq!(ours.overlap(&theirs), None);
        theirs.whole = Some("blog".into());
        assert_eq!(ours.overlap(&theirs), Some("blog".into()));
        assert_eq!(theirs.overlap(&ours), Some("blog".into()));
    }

    #[test]
    fn child_names_carry_the_parent() {
        assert_eq!(child_of("root", "blog"), "blog");
        assert_eq!(child_of("a", "b"), "a__b");
        assert_eq!(depth("a__b"), 2);
    }
}
