//! Putting entries in place: copies, refs, rows, and removing what moved;
//! and the pushes a move waits for.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use camino::Utf8PathBuf;
use rusqlite::Connection;

use crate::App;
use crate::db;
use crate::error::{ApiResult, HttpError};
use crate::repos::{self, Repo};

use super::check::{home_dir, repo_in, store_of};
use super::entry::{Asked, Change, Entry, Stamp};
use super::{Log, lock};

/// How long a move waits for pushes that began where a tracker was.
pub(super) const PUSHES_DRAIN: Duration = Duration::from_secs(10 * 60);

/// An asked ref a landing found moved when it was put in place: left.
#[derive(Debug, Clone)]
pub(super) struct Skipped {
    pub(super) stamp: Stamp,
    pub(super) home: String,
    pub(super) tracker: String,
    pub(super) reference: String,
}

pub(super) fn behind_of(entry: &Entry, stamp: Stamp, skipped: &[Skipped]) -> Vec<Asked> {
    skipped
        .iter()
        .filter(|skip| skip.stamp == stamp)
        .filter_map(|skip| {
            let step = entry
                .homes
                .get(&skip.home)?
                .refs
                .get(&skip.tracker)?
                .get(&skip.reference)?;
            step.asked.then(|| Asked {
                home: skip.home.clone(),
                tracker: skip.tracker.clone(),
                reference: skip.reference.clone(),
                old: step.old.clone(),
                new: step.new.clone().unwrap_or_default(),
            })
        })
        .collect()
}

pub(super) fn placed_of(app: &App, stamp: Stamp) -> ApiResult<Option<bool>> {
    let db = app.db.lock();
    db::placed(&db, stamp)
}

/// Whether `change` acts on the project there now: the incarnation it was
/// written for (or, when it makes one, no project yet).
pub(super) fn acts(current: Option<&String>, change: &Change) -> bool {
    match &change.made {
        Some(_) => current.is_none() || current == Some(&change.incarnation),
        None => current == Some(&change.incarnation),
    }
}

/// Put `entries` (in stamp order, none of them in place yet) in place
/// together:
///
/// 1. Each tracker moved between stores is copied into its new store, and
///    the commits it takes from the lander's landing follow. Nothing says it
///    is there yet, so nobody writes there yet, and running this again
///    (before step 2 is done) copies again. A project made has whatever a
///    project of its name left on disk removed.
/// 2. Under the db lock: every ref, by compare-and-swap, one transaction per
///    store, all prepared before any is committed; then every row and the
///    stamps marked in place, in one SQLite transaction. Refs are synced
///    before the rows are written, and the rows synced before this returns.
/// 3. What moved is removed where it was, once every push that began there
///    is done; a deleted project's home goes. Then the stamps are marked
///    finished.
///
/// A change written for another incarnation of its project than the one
/// there now does nothing. An error means the rows were not written: the
/// entries are not in place. Once they are, they have landed, and a failure
/// of step 3 is said in [`Applied::unfinished`].
pub(super) fn apply_entries(
    app: &App,
    entries: &[(Stamp, &Entry)],
    log: &Log,
    note: &dyn Fn(String),
) -> ApiResult<Applied> {
    if entries.is_empty() {
        return Ok(Applied::default());
    }
    let data = &app.config.data;
    // Which changes act: the project there is the one each was written for,
    // as the entries before it leave it.
    let mut acting: Vec<BTreeSet<String>> = Vec::new();
    {
        let db = app.db.lock();
        let mut current: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (_, entry) in entries {
            let mut here = BTreeSet::new();
            for (home, change) in &entry.homes {
                if !current.contains_key(home) {
                    current.insert(
                        home.clone(),
                        db::project(&db, home)?.map(|info| info.incarnation),
                    );
                }
                let now = current.get(home).cloned().flatten();
                if acts(now.as_ref(), change) || (change.removed && now.is_none()) {
                    here.insert(home.clone());
                }
                if change.removed {
                    current.insert(home.clone(), None);
                } else if change.made.is_some() {
                    current.insert(home.clone(), Some(change.incarnation.clone()));
                }
            }
            acting.push(here);
        }
    }
    // 1. Copies, before anything says they are there.
    let mut moving: Vec<Moving> = Vec::new();
    for ((stamp, entry), here) in entries.iter().zip(&acting) {
        for (home, change) in &entry.homes {
            if !here.contains(home) || change.removed {
                continue;
            }
            if change.made.is_some() {
                let exists = db::project(&app.db.lock(), home)?.is_some();
                if !exists {
                    // A deleted project's name is free again: start from
                    // nothing on disk too.
                    repos::delete(&home_dir(data, home)?)?;
                }
            }
            for (tracker, step) in &change.unmoves {
                let Some(from) = &step.from else {
                    continue;
                };
                repo_in(data, home, Some(from), tracker)?.uncopy(&store_of(&step.to)?)?;
            }
            for (tracker, step) in &change.stores {
                let Some(from) = &step.from else {
                    // Made there: the repo was made before it landed.
                    continue;
                };
                note(format!(
                    "landing {stamp} is copying {tracker} into the {} store",
                    step.to
                ));
                let to = store_of(&step.to)?;
                let repo = repo_in(data, home, Some(from), tracker)?;
                let copied = repo.copy_to(&to)?;
                log.fault("during-copy");
                let borrow = if repos::store_exists(&repo.dir()) {
                    repo.clone()
                } else {
                    copied.to.clone()
                };
                for commit in change.takes.get(tracker).into_iter().flatten() {
                    copied.to.take_landing(&entry.by, commit, &borrow)?;
                }
                moving.push((*stamp, home.clone(), tracker.clone(), repo, copied));
            }
        }
    }
    // 2. Refs, then rows, under the db lock.
    let mut skipped = Vec::new();
    {
        let mut db = app.db.lock();
        // Store, then full ref name: its first old value, its last new one,
        // and every landing's move of it.
        let mut refs: BTreeMap<Utf8PathBuf, BTreeMap<String, Chain>> = BTreeMap::new();
        // Where each tracker is as the landings before leave it.
        let mut stores: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
        for ((stamp, entry), here) in entries.iter().zip(&acting) {
            for (home, change) in &entry.homes {
                if !here.contains(home) || change.removed {
                    continue;
                }
                for (tracker, step) in &change.stores {
                    stores.insert((home.clone(), tracker.clone()), Some(step.to.clone()));
                }
                for (tracker, moved) in &change.refs {
                    let key = (home.clone(), tracker.clone());
                    let store = match stores.get(&key) {
                        Some(store) => store.clone(),
                        None => db::tracker_store(&db, home, tracker)?,
                    };
                    let repo = repo_in(data, home, store.as_deref(), tracker)?;
                    let at = refs.entry(repo.dir()).or_default();
                    for (reference, step) in moved {
                        let chain = at.entry(repo.full(reference)).or_insert_with(|| Chain {
                            old: step.old.clone(),
                            new: step.new.clone(),
                            moves: Vec::new(),
                        });
                        chain.new.clone_from(&step.new);
                        chain.moves.push(Skipped {
                            stamp: *stamp,
                            home: home.clone(),
                            tracker: tracker.clone(),
                            reference: reference.clone(),
                        });
                    }
                }
            }
        }
        let zero = "0000000000000000000000000000000000000000";
        let mut prepared = Vec::new();
        for (dir, chains) in &refs {
            let names: Vec<String> = chains.keys().cloned().collect();
            let now = repos::resolve(dir, &names)?;
            let mut lines = Vec::new();
            for (reference, chain) in chains {
                let current = now.get(reference);
                if current == chain.new.as_ref() {
                    continue;
                }
                if current != chain.old.as_ref() {
                    // Moved by someone else since it was checked: left as
                    // they left it, never overwritten.
                    eprintln!(
                        "newgit serve: {reference} in {dir} is at {}, not {} as checked; left as it is",
                        current.map_or("nothing", String::as_str),
                        chain.old.as_deref().unwrap_or("nothing")
                    );
                    skipped.extend(chain.moves.iter().cloned());
                    continue;
                }
                let old = chain.old.as_deref().unwrap_or(zero);
                lines.push(match &chain.new {
                    Some(new) => format!("update {reference} {new} {old}"),
                    None => format!("delete {reference} {old}"),
                });
            }
            if !lines.is_empty() {
                prepared.push(repos::Prepared::new(dir, &lines)?);
            }
        }
        // Every store's transaction prepared: now committed, one by one.
        let several = prepared.len() > 1;
        for (index, transaction) in prepared.into_iter().enumerate() {
            transaction.commit()?;
            if several && index == 0 {
                log.fault("between-store-commits");
            }
        }
        log.fault("before-rows");
        db.execute_batch("PRAGMA synchronous = FULL")?;
        let written = write_rows(&mut db, entries, &acting);
        if let Err(error) = db.execute_batch("PRAGMA synchronous = NORMAL") {
            eprintln!("newgit serve: setting SQLite back to synchronous=NORMAL: {error}");
        }
        written?;
        app.trackers_moved();
    }
    // 3. What moved, removed where it was: the entries are in place, so
    // whatever fails here leaves them landed and not finished.
    let unfinished = remove_moved(app, entries, &acting, &moving, note).err();
    Ok(Applied {
        skipped,
        unfinished,
    })
}

/// What [`apply_entries`] put in place.
#[derive(Default)]
pub(super) struct Applied {
    /// Asked refs found moved, and left.
    pub(super) skipped: Vec<Skipped>,
    /// Why removing what moved from where it was failed: the entries are in
    /// place, and not finished.
    pub(super) unfinished: Option<HttpError>,
}

/// A tracker being moved by landing `stamp`: home, tracker, where it was,
/// and what was copied.
pub(super) type Moving = (Stamp, String, String, Repo, repos::Copied);

/// Step 3 of [`apply_entries`]: what moved, removed where it was, once
/// every push that began there is done; a deleted project's home removed;
/// the stamps marked finished.
pub(super) fn remove_moved(
    app: &App,
    entries: &[(Stamp, &Entry)],
    acting: &[BTreeSet<String>],
    moving: &[Moving],
    note: &dyn Fn(String),
) -> ApiResult<()> {
    let mut done = BTreeSet::new();
    for (stamp, home, tracker, repo, copied) in moving {
        note(format!(
            "landing {stamp} is waiting for pushes into {tracker} in the {} store to end before removing it there",
            repo.store.name()
        ));
        app.pushes
            .drain(home, tracker, &repo.store.name(), PUSHES_DRAIN);
        repo.finish_move(copied)?;
        done.insert(*stamp);
    }
    for ((stamp, entry), here) in entries.iter().zip(acting) {
        for (home, change) in &entry.homes {
            if here.contains(home) && change.removed {
                remove_home(app, home)?;
            }
        }
        if entry.moves_storage() {
            done.insert(*stamp);
        }
    }
    if !done.is_empty() {
        let db = app.db.lock();
        for stamp in done {
            db::set_placed(&db, stamp, true)?;
        }
    }
    Ok(())
}

/// One ref's moves in a batch.
pub(super) struct Chain {
    pub(super) old: Option<String>,
    pub(super) new: Option<String>,
    pub(super) moves: Vec<Skipped>,
}

/// Every row `entries` change, and each stamp marked in place (finished,
/// unless storage is left to remove), in one SQLite transaction.
pub(super) fn write_rows(
    db: &mut Connection,
    entries: &[(Stamp, &Entry)],
    acting: &[BTreeSet<String>],
) -> ApiResult<()> {
    let transaction = db.transaction()?;
    for ((stamp, entry), here) in entries.iter().zip(acting) {
        for (home, change) in &entry.homes {
            if !here.contains(home) {
                continue;
            }
            if change.removed {
                db::delete_project(&transaction, home)?;
                continue;
            }
            if let Some(made) = &change.made
                && db::project(&transaction, home)?.is_none()
                && !db::create_incarnation(
                    &transaction,
                    home,
                    &made.owner,
                    &made.name,
                    &change.incarnation,
                )?
            {
                return Err(HttpError::new(
                    500,
                    format!("project `{home}` could not be made"),
                ));
            }
            for (branch, list) in &change.branches {
                db::set_branch(&transaction, home, branch, list, &entry.by, *stamp)?;
            }
            for (tracker, step) in &change.stores {
                db::set_tracker_store(&transaction, home, tracker, &step.to)?;
            }
            if let Some(epoch) = change.epoch {
                db::set_epoch(&transaction, home, epoch)?;
            }
            if change.whole {
                // Its whole state: rows it does not name go (a void's, of
                // anything a voided landing could have left).
                let stores = change
                    .stores
                    .iter()
                    .map(|(tracker, step)| (tracker.clone(), step.to.clone()))
                    .collect();
                db::keep_only(&transaction, home, &(change.branches.clone(), stores))?;
            }
        }
        db::set_placed(&transaction, *stamp, !entry.moves_storage())?;
        for voided in &entry.voids {
            db::set_placed(&transaction, *voided, true)?;
            for home in entry.homes.keys() {
                db::set_voided(&transaction, *voided, *stamp, home)?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

/// A deleted project's home removed from disk, unless a project of its name
/// was made again since.
pub(super) fn remove_home(app: &App, home: &str) -> ApiResult<()> {
    let again = db::project(&app.db.lock(), home)?.is_some();
    if !again {
        repos::delete(&home_dir(&app.config.data, home)?)?;
    }
    Ok(())
}

/// Step 3 of [`apply_entries`] for a landing put in place before a restart
/// and not finished: what is left where it moved from is carried forward
/// (never back) and removed there.
pub(super) fn finish(
    app: &App,
    stamp: Stamp,
    entry: &Entry,
    note: &dyn Fn(String),
) -> ApiResult<()> {
    let data = &app.config.data;
    for (home, change) in &entry.homes {
        if change.removed {
            remove_home(app, home)?;
            continue;
        }
        let current = db::project(&app.db.lock(), home)?.map(|info| info.incarnation);
        if current.as_deref() != Some(change.incarnation.as_str()) {
            continue;
        }
        for (tracker, step) in &change.stores {
            let Some(from) = &step.from else {
                continue;
            };
            note(format!(
                "landing {stamp} is removing {tracker} from the {from} store"
            ));
            let was = repo_in(data, home, Some(from), tracker)?;
            let now = repo_in(data, home, Some(&step.to), tracker)?;
            was.finish_move_after_restart(&now)?;
        }
    }
    db::set_placed(&app.db.lock(), stamp, true)?;
    Ok(())
}

/// Pushes running into each tracker's namespace in a store: a move waits
/// for those that began where the tracker was before it removes it there.
#[derive(Default)]
pub struct Pushes {
    pub(super) running: Mutex<BTreeMap<(String, String, String), usize>>,
    pub(super) done: Condvar,
}

/// One push, counted until it is dropped.
pub struct Pushing {
    pub(super) app: Arc<App>,
    pub(super) key: (String, String, String),
}

impl Drop for Pushing {
    fn drop(&mut self) {
        let pushes = &self.app.pushes;
        let mut running = lock(&pushes.running);
        if let Some(count) = running.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                running.remove(&self.key);
            }
        }
        pushes.done.notify_all();
    }
}

impl Pushes {
    /// Count a push into `tracker` of `home` in `store`. Taken under the db
    /// lock, where it was decided which store the push goes to, so a move
    /// either sees it or sent it to the new store.
    pub fn start(app: &Arc<App>, home: &str, tracker: &str, store: &str) -> Pushing {
        let key = (home.to_owned(), tracker.to_owned(), store.to_owned());
        *lock(&app.pushes.running).entry(key.clone()).or_default() += 1;
        Pushing {
            app: app.clone(),
            key,
        }
    }

    /// Wait, at most `limit`, for every push into `tracker` of `home` in
    /// `store` to end.
    pub fn drain(&self, home: &str, tracker: &str, store: &str, limit: Duration) {
        let key = (home.to_owned(), tracker.to_owned(), store.to_owned());
        let running = lock(&self.running);
        let (_running, waited) = self
            .done
            .wait_timeout_while(running, limit, |running| running.contains_key(&key))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if waited.timed_out() {
            eprintln!(
                "newgit serve: a push into {tracker} of {home} in {store} ran past {} s while it moved; what it leaves there stays",
                limit.as_secs()
            );
        }
    }
}
