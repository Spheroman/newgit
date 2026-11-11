//! The check under the lock: what a landing relies on, verified before it
//! is written down.

use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::Connection;

use crate::db::{self, List};
use crate::error::{ApiResult, HttpError};
use crate::names::parse_project_key;
use crate::repos::{self, Repo, Store};

use super::entry::{Asked, Entry, FORMAT, Landing, RefMove};

/// What the landings checked before this one in the batch leave, so each is
/// checked against them as well as against what is there.
#[derive(Default)]
pub(super) struct Seen {
    pub(super) lists: BTreeMap<(String, String), Option<List>>,
    pub(super) stores: BTreeMap<(String, String), Option<String>>,
    /// Each home's incarnation, `None` when there is no project.
    pub(super) projects: BTreeMap<String, Option<String>>,
    /// Homes made or removed in the batch: nothing of SQLite's or the stores'
    /// is theirs.
    pub(super) fresh: BTreeSet<String>,
    pub(super) refs: BTreeMap<(String, String, String), Option<String>>,
    /// Each home's epoch as the landings before this one in the batch leave
    /// it.
    pub(super) epochs: BTreeMap<String, u64>,
    /// What each store answered for each name asked of it this batch
    /// (`None`: missing), so each store is read once per batch, not once per
    /// landing.
    pub(super) resolved: BTreeMap<Utf8PathBuf, BTreeMap<String, Option<String>>>,
}

impl Seen {
    /// [`repos::resolve`] for `names` in the store at `dir`, reading only
    /// what this batch has not read there yet.
    pub(super) fn resolve(
        &mut self,
        dir: &Utf8Path,
        names: &[String],
    ) -> ApiResult<BTreeMap<String, String>> {
        let known = self.resolved.entry(dir.to_owned()).or_default();
        let missing: Vec<String> = names
            .iter()
            .filter(|name| !known.contains_key(*name))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let found = repos::resolve(dir, &missing)?;
            for name in missing {
                let value = found.get(&name).cloned();
                known.insert(name, value);
            }
        }
        Ok(names
            .iter()
            .filter_map(|name| Some((name.clone(), known.get(name)?.clone()?)))
            .collect())
    }

    /// Read ahead, in one go per store, every ref and commit the landings
    /// in `landings` will ask of the stores their trackers are in now.
    pub(super) fn read_ahead<'l>(
        &mut self,
        data: &Utf8Path,
        db: &Connection,
        landings: impl IntoIterator<Item = &'l Landing>,
    ) -> ApiResult<()> {
        let mut names: BTreeMap<Utf8PathBuf, BTreeSet<String>> = BTreeMap::new();
        let mut want =
            |home: &str, store: Option<&str>, tracker: &str, name: String| -> ApiResult<()> {
                let repo = repo_in(data, home, store, tracker)?;
                let name = if name.starts_with("refs/") {
                    repo.full(&name)
                } else {
                    name
                };
                names.entry(repo.dir()).or_default().insert(name);
                Ok(())
            };
        for landing in landings {
            for (home, change) in &landing.entry.homes {
                if home_dir(data, home).is_err() {
                    continue;
                }
                for (tracker, moved) in &change.refs {
                    let now = db::tracker_store(db, home, tracker)?;
                    let to = change.stores.get(tracker).map(|step| step.to.clone());
                    for (reference, step) in moved {
                        want(home, now.as_deref(), tracker, reference.clone())?;
                        if let Some(new) = &step.new {
                            want(home, now.as_deref(), tracker, new.clone())?;
                            if to.is_some() {
                                want(home, to.as_deref(), tracker, new.clone())?;
                            }
                        }
                    }
                }
            }
            for step in &landing.asked {
                if home_dir(data, &step.home).is_err() {
                    continue;
                }
                let now = db::tracker_store(db, &step.home, &step.tracker)?;
                want(
                    &step.home,
                    now.as_deref(),
                    &step.tracker,
                    step.reference.clone(),
                )?;
                want(&step.home, now.as_deref(), &step.tracker, step.new.clone())?;
            }
        }
        for (dir, names) in names {
            let names: Vec<String> = names.into_iter().collect();
            self.resolve(&dir, &names)?;
        }
        Ok(())
    }

    pub(super) fn incarnation(&self, db: &Connection, home: &str) -> ApiResult<Option<String>> {
        match self.projects.get(home) {
            Some(known) => Ok(known.clone()),
            None => Ok(db::project(db, home)?.map(|info| info.incarnation)),
        }
    }

    pub(super) fn epoch(&self, db: &Connection, home: &str) -> ApiResult<u64> {
        match self.epochs.get(home) {
            Some(epoch) => Ok(*epoch),
            None if self.fresh.contains(home) => Ok(0),
            None => db::epoch(db, home),
        }
    }

    pub(super) fn list(
        &self,
        db: &Connection,
        home: &str,
        branch: &str,
    ) -> ApiResult<Option<List>> {
        match self.lists.get(&(home.to_owned(), branch.to_owned())) {
            Some(list) => Ok(list.clone()),
            None if self.fresh.contains(home) => Ok(None),
            None => db::list_of(db, home, branch),
        }
    }

    pub(super) fn store(
        &self,
        db: &Connection,
        home: &str,
        tracker: &str,
    ) -> ApiResult<Option<String>> {
        match self.stores.get(&(home.to_owned(), tracker.to_owned())) {
            Some(store) => Ok(store.clone()),
            None if self.fresh.contains(home) => Ok(None),
            None => db::tracker_store(db, home, tracker),
        }
    }
}

pub(super) fn home_dir(data: &Utf8Path, home: &str) -> ApiResult<Utf8PathBuf> {
    parse_project_key(home)
        .map(|id| id.dir(data))
        .ok_or_else(|| HttpError::new(400, format!("`{home}` is not a project")))
}

pub(super) fn store_of(name: &str) -> ApiResult<Store> {
    Store::parse(name).ok_or_else(|| HttpError::new(500, format!("no store `{name}`")))
}

pub(super) fn repo_in(
    data: &Utf8Path,
    home: &str,
    store: Option<&str>,
    tracker: &str,
) -> ApiResult<Repo> {
    let store = store.map(store_of).transpose()?;
    Ok(Repo::tracker(&home_dir(data, home)?, store, tracker))
}

/// The checks under the lock: every project the incarnation the landing was
/// written for, every branch still as it expects, every tracker still where
/// it expects, each asked ref still at its old commit (one moved meanwhile
/// is left, and said), every other ref's old value recorded, and every
/// commit a ref moves to in the store it lands in. What this batch lands
/// before it counts.
pub(super) fn check(
    data: &Utf8Path,
    db: &Connection,
    landing: Landing,
    seen: &mut Seen,
) -> ApiResult<(Entry, Vec<Asked>)> {
    let Landing {
        mut entry,
        expect,
        placed,
        asked,
        epochs,
    } = landing;
    entry.format = FORMAT;
    for (home, change) in &entry.homes {
        home_dir(data, home)?;
        let now = seen.incarnation(db, home)?;
        if change.made.is_some() {
            if now.is_some() {
                return Err(HttpError::new(
                    409,
                    format!("project `{home}` already exists"),
                ));
            }
        } else if now.as_deref() != Some(change.incarnation.as_str()) {
            return Err(HttpError::new(
                409,
                match now {
                    None => format!("project `{home}` was deleted since you looked"),
                    Some(_) => format!(
                        "project `{home}` was deleted and made again since you looked: start again"
                    ),
                },
            ));
        }
        for step in change.stores.values() {
            store_of(&step.to)?;
            if let Some(from) = &step.from {
                store_of(from)?;
            }
        }
    }
    for want in &expect {
        let now = seen.list(db, &want.home, &want.branch)?;
        if now != want.list {
            return Err(match (&want.list, now) {
                (None, Some(_)) => HttpError::new(
                    409,
                    format!(
                        "branch `{}` exists; a branch moves only through `newgit merge`",
                        want.branch
                    ),
                ),
                _ => HttpError::new(
                    409,
                    format!(
                        "`{}` moved since you looked: merge again onto what it is now",
                        want.branch
                    ),
                ),
            });
        }
    }
    for (home, epoch) in &epochs {
        if seen.epoch(db, home)? != *epoch {
            return Err(HttpError::new(
                409,
                format!(
                    "the definitions of `{home}` changed since this merge was prepared: start again"
                ),
            ));
        }
    }
    for (home, change) in &entry.homes {
        if let Some(epoch) = change.epoch
            && epoch != seen.epoch(db, home)? + 1
        {
            return Err(HttpError::new(
                409,
                format!(
                    "the definitions of `{home}` changed since this merge was prepared: start again"
                ),
            ));
        }
    }
    for want in &placed {
        if seen.store(db, &want.home, &want.tracker)? != want.store {
            return Err(HttpError::new(
                409,
                format!(
                    "`{}` moved to another store since you looked: merge again",
                    want.tracker
                ),
            ));
        }
    }
    // Every ref it moves, as it is now: where the batch left it, or read
    // from the store the tracker is in before this landing, one read per
    // store.
    let mut wanted: BTreeSet<(String, String, String)> = BTreeSet::new();
    for (home, change) in &entry.homes {
        for (tracker, moved) in &change.refs {
            for reference in moved.keys() {
                wanted.insert((home.clone(), tracker.clone(), reference.clone()));
            }
        }
    }
    for step in &asked {
        if !entry.homes.contains_key(&step.home) {
            return Err(HttpError::internal(format!(
                "a landing asks a ref in `{}`, which its entry does not name",
                step.home
            )));
        }
        wanted.insert((
            step.home.clone(),
            step.tracker.clone(),
            step.reference.clone(),
        ));
    }
    let now = read_refs(data, db, seen, &wanted)?;
    let mut behind = Vec::new();
    for (home, change) in &mut entry.homes {
        for (tracker, moved) in &mut change.refs {
            for (reference, step) in moved {
                step.old = now
                    .get(&(home.clone(), tracker.clone(), reference.clone()))
                    .cloned()
                    .flatten();
                step.asked = false;
            }
        }
    }
    for step in asked {
        let key = (
            step.home.clone(),
            step.tracker.clone(),
            step.reference.clone(),
        );
        let current = now.get(&key).cloned().flatten();
        if current.as_deref() == Some(step.new.as_str()) {
            continue;
        }
        if current != step.old {
            behind.push(step);
            continue;
        }
        entry
            .homes
            .entry(step.home.clone())
            .or_default()
            .refs
            .entry(step.tracker.clone())
            .or_default()
            .insert(
                step.reference.clone(),
                RefMove {
                    old: step.old.clone(),
                    new: Some(step.new.clone()),
                    asked: true,
                },
            );
    }
    // Every commit a ref moves to is in the store it lands in (or comes with
    // the tracker's move, or waits in the lander's landing for it).
    let mut needs: BTreeMap<Utf8PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut whose: Vec<(Utf8PathBuf, Option<Utf8PathBuf>, String, String, bool)> = Vec::new();
    for (home, change) in &entry.homes {
        for (tracker, moved) in &change.refs {
            let step = change.stores.get(tracker);
            let to = match step {
                Some(step) => Some(step.to.clone()),
                None => seen.store(db, home, tracker)?,
            };
            let lands = repo_in(data, home, to.as_deref(), tracker)?.dir();
            let from = step
                .and_then(|step| step.from.as_deref())
                .map(|from| repo_in(data, home, Some(from), tracker).map(|repo| repo.dir()))
                .transpose()?;
            for step in moved.values() {
                let Some(new) = &step.new else {
                    continue;
                };
                let taken = change
                    .takes
                    .get(tracker)
                    .is_some_and(|takes| takes.contains(new));
                needs.entry(lands.clone()).or_default().insert(new.clone());
                if let Some(from) = &from {
                    needs.entry(from.clone()).or_default().insert(new.clone());
                }
                whose.push((
                    lands.clone(),
                    from.clone(),
                    tracker.clone(),
                    new.clone(),
                    taken,
                ));
            }
        }
    }
    let mut held: BTreeMap<Utf8PathBuf, BTreeMap<String, String>> = BTreeMap::new();
    for (dir, commits) in needs {
        let commits: Vec<String> = commits.into_iter().collect();
        held.insert(dir.clone(), seen.resolve(&dir, &commits)?);
    }
    for (lands, from, tracker, commit, taken) in whose {
        let has = |dir: &Utf8PathBuf| {
            held.get(dir)
                .is_some_and(|found| found.contains_key(&commit))
        };
        if !(taken || has(&lands) || from.as_ref().is_some_and(has)) {
            return Err(HttpError::new(
                400,
                format!(
                    "`{tracker}`'s commit {commit} is not on the remote: push it to its landing ref, then finish"
                ),
            ));
        }
    }
    // What it leaves, for the ones after it in the batch.
    for (home, change) in &entry.homes {
        if change.removed {
            seen.projects.insert(home.clone(), None);
            seen.fresh.insert(home.clone());
            continue;
        }
        if change.made.is_some() {
            seen.projects
                .insert(home.clone(), Some(change.incarnation.clone()));
            seen.fresh.insert(home.clone());
        }
        for (branch, list) in &change.branches {
            seen.lists
                .insert((home.clone(), branch.clone()), Some(list.clone()));
        }
        for (tracker, step) in &change.stores {
            seen.stores
                .insert((home.clone(), tracker.clone()), Some(step.to.clone()));
        }
        if let Some(epoch) = change.epoch {
            seen.epochs.insert(home.clone(), epoch);
        }
        for (tracker, moved) in &change.refs {
            for (reference, step) in moved {
                seen.refs.insert(
                    (home.clone(), tracker.clone(), reference.clone()),
                    step.new.clone(),
                );
            }
        }
    }
    Ok((entry, behind))
}

/// A ref, as a landing names it: home, tracker, and ref as its readers name
/// it.
pub(super) type RefKey = (String, String, String);

/// Each of `wanted` (home, tracker, ref as its readers name it) as it is:
/// where the batch left it, or read from the store its tracker is in.
pub(super) fn read_refs(
    data: &Utf8Path,
    db: &Connection,
    seen: &mut Seen,
    wanted: &BTreeSet<(String, String, String)>,
) -> ApiResult<BTreeMap<(String, String, String), Option<String>>> {
    let mut out = BTreeMap::new();
    let mut by_store: BTreeMap<Utf8PathBuf, Vec<(RefKey, String)>> = BTreeMap::new();
    for key in wanted {
        if let Some(known) = seen.refs.get(key) {
            out.insert(key.clone(), known.clone());
            continue;
        }
        let (home, tracker, reference) = key;
        if seen.fresh.contains(home) {
            out.insert(key.clone(), None);
            continue;
        }
        let repo = repo_in(
            data,
            home,
            seen.store(db, home, tracker)?.as_deref(),
            tracker,
        )?;
        by_store
            .entry(repo.dir())
            .or_default()
            .push((key.clone(), repo.full(reference)));
    }
    for (dir, keys) in by_store {
        let names: Vec<String> = keys.iter().map(|(_, full)| full.clone()).collect();
        let found = seen.resolve(&dir, &names)?;
        for (key, full) in keys {
            out.insert(key, found.get(&full).cloned());
        }
    }
    Ok(out)
}
